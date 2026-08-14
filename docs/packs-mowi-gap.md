# Go `packs/mowi` retirement gap audit

Status: audit against `/home/subosito/Code/runner/mow/packs/mowi` and the current Rust working tree. This document distinguishes observable parity from intentional product changes; matching Bubble Tea internals is not a goal.

## Executive summary

The current Rust client now covers the former client-side P0 blockers: rewind-backed
edit/retry, cancellation queue handling, reasoning privacy, OSC52 copy, async
compact/rewind/model/effort/permission operations, and local command discovery.

Deleting `packs/mowi` is **not yet safe**. The remaining product blockers are:

1. `/btw` needs an ephemeral prompt option in the RPC protocol.
2. Jailed `@path` expansion needs a host-owned, race-safe RPC implementation.
3. Goal/LSP event UX and curated status/activity parity remain optional P1 work.
4. The `mow` repository still builds, tests, documents, and distributes the Go
   binary from many places.

## P0: behavior to fix before retirement

| Gap | Go behavior | Rust behavior today | Required owner |
|---|---|---|---|
| `/edit` and `/retry` semantics | Rewind the last exchange, remove its painted turn, then refill or regenerate (`tui_commands.go`, `edit_effort_fix_test.go`) | Implemented with async `rewind`; transcript refresh and follow-up are guarded | Complete in Rust client |
| Cancel and queue | Esc/cancel drops queued follow-ups (`tui_update.go`) | Cancel marks the turn and drops queued prompts; cancelled completion cannot drain the queue | Complete in Rust client |
| Reasoning privacy/state | Handles `loop.reasoning`; strips `<think>…</think>`; paints indicator only (`tui_stream.go`, `think_test.go`) | Reasoning events and think-tag filtering are implemented and tested | Complete in Rust client |
| Clipboard | `/copy` emits terminal clipboard/OSC52 (`tui_commands.go`) | Queues OSC52 for the next terminal write; keeps native selection available | Complete in Rust client |
| `/btw` ephemeral aside | Runs against current context without adding the exchange (`tui_commands.go`, `interaction_test.go`) | No RPC prompt option; Rust cannot request an ephemeral turn | **Remaining host blocker:** add RPC prompt semantics, then wire Rust |
| Jailed `@path` expansion | Resolves via Engine jail, caps content at 100 KiB (`fileref.go`) | No RPC contract; client must not read paths outside the host jail | **Remaining host blocker:** add host-owned, race-safe expansion |

## P1: operator-visible gaps

| Area | Remaining gap | Existing support / disposition |
|---|---|---|
| Skills | Go has startup `--skill` and `/skill` | Rust has repeatable `--skill`, `/skills`, help, completion, `skill.list`, and `skill.activate` |
| Goals | Go has in-session `/goal`, header progress, blocked/step state, and `graph.goal.*` handling (`goal.go`) | Decide whether goal operations become registered RPC slash commands; Rust can consume generic event notifications once handlers are added |
| LSP | Go retains diagnostics and offers `/lsp` display | Events can already cross the generic event stream; Rust ignores them |
| Permission mode | Go supports `/perm ask|auto` as well as Shift+Tab | Rust supports both `/perm ask|auto` and Shift+Tab through `perm.set` |
| Status | Go presents a concise status/usage summary including peer share | Rust exposes status and usage, but curation/peer-share presentation is still a parity choice |
| Activity labels | Go maps tools to plain verbs and compact arguments (`label.go`) | Rust labels are less specific in some event forms |
| Compaction | Go defers unsafe timing and responds to compact events | Rust has async `/compact` and refreshes the transcript after completion |
| Discoverability | Help/completions list implemented local commands consistently | Complete for current Rust client; keep in sync if new local commands land |
| Model/effort overlay action | Selection from an already-open picker still uses synchronous calls | Rust picker selections use typed pending-operation requests |
| Permission/ask RPC | Permission decisions must not block the event loop | Rust uses async permission requests and preserves the 200 ms approval guard |
| Streaming flag | Go exposes `--no-stream` | `mow rpc` does not accept `--no-stream` in `ext/rpc/cmd.go`; do not forward an unsupported flag |
| Welcome | Go can show trust state | Rust can derive capability state; decide whether trust itself belongs in RPC/status |

## P2: useful parity, not deletion blockers

- Full diff overlay (`Ctrl+E`) with unified/split views (`diff_overlay.go`). Rust has strong inline review cards but no expanded viewer.
- Search-result highlighting/navigation polish.
- Model-picker filtering and an effort-cycle shortcut.
- Empty-document guidance after dismissing the welcome screen.
- Goal/context-sink/LSP specialized event rows rather than generic notes.
- Context-pressure teaching and queue teaching messages.
- A bounded source-text memory policy for extremely long sessions. Rust virtualizes rendering and caches heights, but retains full source text; Go stubs old source after 80 entries (`gc.go`). Do not copy destructive GC blindly—prefer reloadable transcript pages or a measured memory ceiling.
- PTY/cell smoke coverage. Rust snapshot/TestBackend coverage is good, but a real-terminal geometry smoke would catch terminal protocol differences.
- Resumed-message timestamps. RPC transcript entries currently contain only role/content, so Rust correctly avoids inventing times. Add an optional timestamp to RPC only if this is worth preserving.

## Intentional differences to retain

These are product decisions, not parity defects:

- Rust mowi is an external `mow rpc` client; it must not embed the Engine or implement ACP.
- Pack CLI commands and trust management belong to `mow` (`mow review`, `mow goal`, `mow trust`), not duplicate Rust subcommands.
- Live activity is consolidated into the status bar rather than a separate activity band.
- Arrow/PgUp scrolling and `/edit` are explicit; Ctrl+U/Ctrl+D remain unbound.
- Mouse capture remains off for native selection; no Go-style Ctrl+S select mode is needed.
- Composer remains focused; no separate Ctrl+O transcript-focus mode is needed.
- The compact, content-bounded Help and peer overlays supersede Go's taller overlays.

## Rust advantages already established

- UI and Engine are cleanly separated by RPC.
- Input-repeat backlog and event-source starvation are bounded.
- Long transcript painting, height estimation, and live-tail rendering are virtualized/cached.
- Idle frames are not redrawn at 20 FPS; context refresh and common local RPC commands are asynchronous.
- Header/status/composer hierarchy is more compact and responsive.
- Native terminal copy/select works without a mode toggle.
- Snapshot scenes cover narrow, normal, colored, and `NO_COLOR` layouts.

## RPC work required versus optional

### Required

- **Ephemeral prompt semantics for `/btw`:** update `mow/ext/rpc/rpc.go`
  (`handlePrompt` to decode an `ephemeral` option and call `Engine.PromptWith`)
  and document the request shape in `mow/ext/rpc/README.md`. Add an RPC
  regression beside `mow/ext/rpc/rpc_internal_test.go`'s prompt tests and an
  end-to-end host assertion in `mow/ext/rpc/host_test.go`; preserve the engine
  contract covered by `mow/internal/engine/engine_ephemeral_test.go`.
- **Engine-owned, jail-safe `@path` expansion:** add the expansion/read
  contract at the RPC boundary in `mow/ext/rpc/rpc.go` (prefer a small
  `mow/ext/rpc/fileref.go` helper), using the Engine/policy jail and a
  symlink-race-safe bounded read. Cover allowed workspace/extra-root files,
  `..`/absolute escapes, symlink escapes, missing files, and the 100 KiB cap
  in `mow/ext/rpc/host_test.go`; reuse or strengthen the lower-level guarantees
  in `mow/internal/tools/jailfile_test.go` and
  `mow/internal/policy/policy_test.go` as needed.

### Existing RPC that only needs Rust wiring

- `rewind`, `skill.list`, `skill.activate`, `perm.set`, model/effort methods
  are wired in the current Rust client.
- Generic `event` transport for reasoning, LSP, compact, goal, and context-sink events, subject to confirming those event types are emitted by the RPC host.

### Optional additive RPC

- Optional transcript timestamp.
- TUI config payload if Rust should consume shared `extensions.tui`; alternatively define a Rust-owned config and keep RPC out of it.
- Goal-specific methods only if goal commands cannot be represented through the registered slash surface.

## Repository deletion checklist

Removing the directory alone will break the `mow` repository. After the two
required RPC contracts and the Rust regression coverage land:

1. Remove the nested module from `go.work` and delete `packs/mowi/go.mod` /
   `go.sum` with the package.
2. Replace `.github/workflows/ci.yml` jobs that run `cd packs/mowi && go
   vet/test/build` with Rust mowi CI or a cross-repository release check.
3. Update `justfile` targets (`build-mowi`, `test-mowi`, `race-mowi`, release
   packaging) and remove the nested-module build assumptions.
4. Remove or redirect `scripts/smoke-tui.sh` and
   `scripts/smoke-diff-cells.sh`, including their `bin/mowi` checks.
5. Rewrite Go-TUI references in `README.md`, `AGENTS.md`, `CONTRIBUTING.md`,
   `docs/architecture.md`, `docs/extensions.md`, and `docs/harness.md`.
6. Update extension READMEs that say `packs/mowi/cmd/mowi` blank-imports
   packs; `cmd/mow` becomes the sole full pack host.
7. Decide installation/release ownership for the Rust binary (package name,
   artifacts, versioning, and how `mow` locates or recommends it).
8. Preserve the Go tests under `packs/mowi` as migration specifications until
   each retained behavior has a Rust or RPC regression test; then delete the
   nested module and generated build references together.
9. Run root Go tests, RPC tests, Rust tests, and one live cross-process smoke
   covering resume, ask/auto, cancellation, queued prompts, steer, compact,
   and long scrollback before removal.

## Suggested retirement sequence

1. Add RPC contracts for ephemeral prompts and jail-safe file references, with
   host and security regression tests.
2. Decide whether goal/LSP/status/activity parity is required for retirement;
   otherwise record those as intentional omissions.
3. Add cross-process smoke tests covering resume, ask/auto, cancellation,
   queued prompts, steer, compact, skills, and long scrollback.
4. Freeze `packs/mowi` except for parity-reference fixes.
5. Migrate build/docs/release references and remove `packs/mowi` in one final
   mow commit.
