# packs/mowi — design notes

How the Go TUI is built: the elements it paints, the decisions behind them,
and the failure modes those decisions exist to prevent.

This is a description of the current implementation, written for someone who
has to change it (or reimplement it — see [Porting](#porting-notes) at the
end). It is not a user manual; for keys and slash commands read the help
overlay (`?`).

## What mowi is

`mowi` is "mow with interface": a Bubble Tea front end over the headless
`mow.Engine`. Import path `github.com/subosito/mow/packs/mowi`, config section
`extensions.tui`, shared `MOW_HOME` with `mow`.

**It does not implement the agent loop.** Every piece of work goes through
`mow.Engine` — prompting, tools, sessions, model switching. mowi owns
presentation and interaction only. This is the single most important
constraint in the package: when a behavior could live in the engine or in the
UI, it lives in the engine, and mowi renders the result.

Dependency direction is one-way: `packs/mowi` (nested module) → root public
API. TUI dependencies never move into the root module or `internal/engine`.
That is why mowi is a separate Go module with its own `go.mod` — a headless
integrator embedding `mow` must not transitively pull Bubble Tea, Lip Gloss,
Chroma and Goldmark.

## Element inventory

| Element | Source | Role |
|---|---|---|
| Header chip line | `tui_chrome.go` | identity (model/effort), safety chips, context gauge |
| Activity band | `tui_chrome.go`, `label.go` | one live line: spinner, elapsed, current tool |
| Transcript viewport | `tui_transcript.go`, `virtual.go` | scrollback of entries |
| Input textarea | `tui.go`, `tui_chrome.go` | multi-line draft, prompt prefix |
| Status line | `tui_chrome.go` | transient one-shot messages |
| Overlays | `overlay.go`, `model_pick.go`, `effort_pick.go`, `diff_overlay.go` | modal pickers and viewers |
| Permission prompt | `tui_perm.go` | y/n/a gate for power tools |

Third-party building blocks: `bubbles/v2` (`textarea`, `viewport`, `spinner`,
`key`), `bubbletea/v2` (runtime), `lipgloss/v2` (styling), `chroma/v2` (syntax
highlighting + theme palettes), `goldmark` (markdown parse), `x/ansi`
(width-correct ANSI handling), `termenv` (background detection).

## Layout

Top to bottom: **header · hairline · [activity band] · transcript · input ·
footer**. The activity band only exists while a turn runs; the transcript
absorbs the freed rows when it disappears.

The header is a **priority-dropped chip line**. As the terminal narrows,
chips are dropped in reverse priority order rather than wrapped or truncated
mid-word: identity survives longest, decorative chips go first. The context
gauge is suppressed entirely below `ctxGaugeMinWidth` (100 columns) because narrow
terminals need every column for identity and safety chips — the numeric
percentage alone still carries the signal there.

## Transcript model

The transcript is a list of **entries**, each with a kind (user, assistant,
tool, status, error, …). Roles are indicated by a colored **gutter bar** and a
soft background fill on user blocks, not by text labels like "User:". The
timestamp is an inline muted stamp at the start of a user block, not a
separate row. Rationale: labels and timestamp rows cost a line each and turn
a conversation into a form; color and indentation carry the same information
at zero vertical cost.

### Virtualization

Long sessions must not hold a fully styled ANSI string for every entry.
`virtual.go` keeps source text but drops rendered `.view` caches outside a
margin around the viewport, contributing blank-line placeholders so the
document's total height — and therefore scrollbar position and `YOffset`
math — stays exact.

Three windows govern this:

- `prettyWindow` (48): recent entries always fully rendered, markdown included.
- `scrollPrettyRadius` (16): entries near the scroll position get upgraded to
  full render as you move.
- `keepViewRadius` (24): `.view` is cleared outside this margin.

### Text GC

`gc.go` goes further: beyond `entryTextKeepFull` (80) entries, older
user/assistant *source text* is stubbed to `entryTextStubRunes` (96). Status
and tool lines are cheap and kept as-is.

There is a subtle guard here worth preserving: **GC is skipped until
`m.ready`**. `seedTranscript` runs before the first `WindowSize` message, when
there is no viewport, so every entry looks off-screen — without the guard a
resumed session would stub its own history into unreadable markers before the
user could scroll to it. This is the kind of bug that only appears on resume,
which is exactly why the guard is commented in place.

## Streaming and event handling

Token deltas arrive on engine callbacks and are committed to the transcript by
`Update`. Two decisions matter:

**Peer/ACP deltas are batched outside Bubble Tea's channel.**
`peerDeltaIngest` (`tui_stream.go`) accumulates chunks under a mutex; `Update`
drains it on a paint heartbeat and before committing an `endPeer` event. The
reason is stated in the code: Bubble Tea's message channel is bounded, and the
ACP event callback must never drop model output or block the engine goroutine.
Pushing every delta as a `tea.Msg` risks both.

**The busy heartbeat owns its own tick chain.** `scheduleBusyHeartbeat`
advances the spinner frame and the elapsed counter; `advanceSpinnerFrame` uses
`tag=0` and discards the bubbles spinner's own follow-up `Cmd`. This is a
deliberate workaround: bubbles' tag-based `Tick` reschedule returns a nil
`Cmd` on tag mismatch, and the animation then dies permanently. Owning the
chain makes the heartbeat unconditional.

The elapsed counter is **always visible while busy**, not just the spinner.
A spinner frame can stall; a monotonically increasing timer is the honest
signal that distinguishes "thinking" from "hung".

## Tool display

Per-call tool lines flood the transcript, so a turn gets **one tool tally
line** that is rewritten in place (`bumpToolTally`). A single call keeps the
richer `name · 0.4s` form; repeats collapse to counts. Failures fold into the
same line (`bumpToolError`) rather than stacking a red row each — this was a
concrete fix for `line_hash` misses flooding the UI.

`label.go` builds activity labels as `verb · basename`, never
mid-string-truncating a shell blob into noise. Labels add a broad phase
without hiding the concrete operation: `searching · grep · activityState`
keeps both the state vocabulary and the actual argument, so the band never
degrades into an opaque animation.

## Theming and color

**The background is probed once, before Bubble Tea owns the TTY.**
`pinTerminalTheme` resolves light/dark and caches it. Calling
`termenv.HasDarkBackground()` *during* the TUI issues OSC 11 queries whose
replies leak into the input stream as garbage like `]11;rgb:1e1e/1e1e/2e2e`
and can block the UI for seconds. Lip Gloss v2 removed
`SetHasDarkBackground`, so mowi pins the value and builds fixed-color styles
from it (`newThemeFrom`).

Themes resolve to a Chroma style name (or `default`), which gives syntax
highlighting and UI chrome a shared palette for free.

The theme is intentionally quiet: **transcript first, chrome second.** Diffs
are the exception — they are rendered as a code-review card with tinted rows
and a gutter rather than raw git output, and the add/delete colors were tuned
deliberately for contrast. `DiffAddSoft`/`DiffDelSoft` let unchanged word
segments on a changed row recede so the actual edit stands out within the
line.

## Context pressure

`formatContextPctLevel` has three levels — muted, attention (≥50%), warn
(≥80%) — and `ctxGauge` renders a small fill bar matched to those thresholds.
The gauge uses `ViewAs` so it is a pure function of the ratio: no animation
state, no `Update` wiring, no frame `Cmd`s.

The percentage is the source of truth and the bar is additive; when the ratio
is unusable `ctxGauge` returns `""` and the caller falls back to the number
alone. `formatContextPct` avoids integer-floor "0% ctx" on large windows.

## Input

A `textarea` for multi-line drafts, with a prompt prefix that changes with
state: spinner plus elapsed while busy, `❯` when idle, amber when the draft
is a slash command.

`@path` tokens are expanded into the prompt by `fileref.go`, capped at
`maxRefBytes` (100 KB) per file and resolved through `Engine.ResolvePath` —
the same workspace path jail the FS tools use. Expansion cannot reach outside
the jail.

Draft text is scrubbed with `mouseLeakRe` for SGR mouse reports. Terminals
leak these into stdin in some configurations, and without the scrub they land
in the user's prompt as `[<35;80;24M` noise.

## Slash commands and overlays

Slash commands split three ways:

1. **Local UI commands** — `/help`, `/clear`, `/model`, `/effort`, and so on.
2. **Pack commands** (`packslash.go`) — `review`, `sec`, contributed by linked
   packs and discovered at runtime, so removing a pack import removes the
   command.
3. **Skills** (`skill_slash.go`) — workspace-defined.

Pickers (`model_pick.go`, `effort_pick.go`) are overlays over the transcript
rather than inline prompts, so opening one never rewrites scrollback.

## Testing strategy

The package is roughly half tests (~26k lines total, with `tui_test.go` alone
at 2.2k). Two ideas carry most of the weight:

**Assert on the painted grid, not on returned strings.** `just smoke-tui`
drives the real `bin/mowi` in a PTY and checks per-cell char/fg/bg. This is
the only thing that catches column geometry: the diff sign glyphs once sat 6
columns apart with every Go unit test passing.

**Test the invariants that broke before.** `diff_readability_test.go`,
`diffcontrast_test.go`, `ctx_gauge_test.go`, `mouse_scroll_test.go`,
`resume_scroll_test.go` and `display_sanitize_test.go` each exist because a
specific regression shipped. They encode contrast ratios, gauge thresholds and
scroll behavior as assertions rather than as review comments.

`smoke-tui` needs a model endpoint and a network round trip, so CI cannot run
it — it is opt-in, not part of `verify`. Run it when touching rendering.

## Porting notes

For a reimplementation (e.g. the Ratatui client driving `mow rpc` over
JSON-RPC), the decisions that transfer are the ones tied to terminals and
agents rather than to Bubble Tea:

- Probe terminal background **once**, before entering raw mode.
- Virtualize the transcript with exact total height; keep source, drop styled
  caches.
- Guard history GC until the first size event, or resume eats its own
  scrollback.
- Batch high-rate deltas outside the UI event channel.
- Keep an always-visible elapsed counter, not just a spinner.
- One rewritten tool tally line per turn; fold errors into it.
- Priority-drop header chips; suppress the gauge before truncating identity.
- Scrub mouse-report escapes out of draft text.

The RPC surface exposes the engine capabilities this UI uses directly —
`model.list`/`model.set`, `effort.list`/`effort.set`, `context`, `compact`,
`rewind`, `skill.list`/`skill.activate`, plus `slash.list`/`slash` for pack
commands. See `ext/rpc/README.md`. That surface was derived by auditing which
`mow.Engine` methods this package calls, so parity with mowi is the design
target.

## Gotchas

- Bubble Tea's message channel is bounded — never push per-token messages
  from a hot callback.
- The bubbles spinner's tag-based reschedule dies silently on tag mismatch.
- OSC 11 during the TUI leaks into stdin and can block for seconds.
- `seedTranscript` runs before the first `WindowSize`.
- `x/ansi` for widths: raw `len()` on styled strings breaks layout on CJK and
  emoji.
