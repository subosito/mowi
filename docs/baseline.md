# Baseline — what Go mowi already is

Source of truth: `mow/packs/mowi`. This crate should reach **the same
operator experience**, not a subset that forgets permissions or resume.

## Screen (document, not chat log)

| Region | Behavior |
|---|---|
| Header | Left: `mowi`, workspace basename, `model (effort)`. Right: safety chips (write/shell, ask/auto), then optional git / extra-root / status-bar Goal states, then the token chip, then the context size (`32k/128k ctx`, or `32k ctx` if the window is unknown) at the far right. A ` · ` joins safety to the first optional chip and is omitted when none remain. Drop order (first to go): tokens, extra-roots, context size, then identity (workspace, effort, model). Safety never drops. The git chip is a cached local probe of the RPC workspace (startup + debounce after mutating tools / turn end; never per frame; hidden outside a worktree). Extra-root chips appear only when `status`/`session` sent those fields. The status-bar Goal state is driven only by confirmed `graph.goal.*` events and clears after `done`/`failed` on the next user prompt. Session id is never a header or status-bar chip; the help overlay titles the full id. |
| Transcript | User blocks (soft fill), assistant markdown, one compact tool line per call — a turn's calls collapse into a counted row (`⚙ bash ×2 · grep`) that shortens by whole tokens on a narrow pane (Esc collapses an expanded group). While busy, a bounded live progress section shows streaming answer tokens, the current/recent tool (verb + path), and write/edit diffs as review cards; it folds into the tool group at turn end. Edits = inline review cards (−/+). |
| Status bar | Idle: `● idle`. Busy: spinner, elapsed, verb (and typing pulse while tokens land) on this row — no separate activity band. Hints flush right and degrade before state does. |
| Input | Sits on the document ground with a horizontal inset and no box. Enter sends; busy queues. `/steer` redirects the running turn. |
| Welcome | Splash, dismisses on any key. Short panes drop the tagline/effort first so access and `type to begin` still fit. |
| Min size | 40×10; below that a size warning, not a broken frame. |

## Input & keys (defaults)

| Key | Action |
|---|---|
| Enter | send (queue if busy) |
| ctrl+j | newline (input grows 1–10 rows) |
| ↑ / ↓ | browse prompt history |
| Esc | dismiss overlay, else collapse tool calls, else cancel turn, else ignore |
| ctrl+l | clear transcript (UI-local; Engine history remains) |
| shift+tab | ask ↔ auto (`perm.set`) |
| ctrl+p | expand the last peer buffer in an overlay |
| ctrl+/ or `?` on empty | help overlay (local keys + `slash.list`) |
| ctrl+c | quit (cancel first if busy) |
| home / end | cursor to start / end of input |
| left / right | move cursor |
| delete | delete forward |
| paste | bracketed paste at the cursor, multi-line safe |
| any key | dismiss the welcome splash |

All bindings remappable later (`extensions.tui.keys` in Go). v1 hard-codes
defaults. Quit is `/quit` / `/exit` / `/q` or ctrl+c — a lone `q` on empty
input does not quit. Mowi captures wheel events for transcript scrolling; hold Shift while dragging for terminal-native selection/copy.

## Commands (typed)

Host-side (this crate, or slash if registered):

| Command | Notes |
|---|---|
| `/steer <text>` | RPC `steer` while busy (async; local error when idle or unsupported) |
| `/status` | RPC `status` + last usage; show peer share |
| `/sessions` | RPC `sessions` — overlay list + `mowi --session <id>` hint |
| `/search` | UI-local find in painted transcript |
| `/copy` `/retry` `/edit` | UI-local / re-`prompt` (`/edit` loads the last user prompt) |
| `/clear` | UI-local: clear painted transcript (engine history kept) |
| `/quit` `/exit` `/q` | UI-local quit (cancels an in-flight turn first) |
| `/model` | UI-local: `model.list` overlay (enter to set), or `/model gpt-5-mini` → `model.set` |
| `/effort` | UI-local: `effort.list` overlay (enter to set), or `/effort high` → `effort.set` |
| `/review` `/sec` `/goal` … | RPC `slash` — only if `slash.list` has them |

Commands fall into three classes. Core local names (`/help`, `/quit`,
`/clear`, `/search`, `/copy`, `/status`) are always offered. RPC-method-gated
names (`/model`, `/effort`, `/steer`, `/compact`, `/skills`, `/lsp`, …)
appear in Help / completion only when `version` / `capabilities` advertised
the backing method, feature, or event. Pack-discovered names stay dynamic
via `slash.list` — `/goal` is never hardcoded. An empty method list does
not infer a stock build. A typed gated command that is not offered is
`/{name} is not available on this host`, not a generic unknown list.

Local commands are routed by name before the RPC fallback, so `/quit`,
`/model`, `/effort`, and friends can never be sent to the host as an unknown
slash command. RPC `slash` is used only for names in the cached `slash.list`.
Anything else is a local error listing *offered* commands.

Exclusive slash: refuse while `status.busy`.

`/review` and `/sec` use the **session model**. They do not start a
`--reviewer` ensemble. Ensemble is `mow review` CLI.

## Permissions

Power tools (`write`, `edit`, `bash`) when ask mode:

```
y allow · n deny · a always (this session)
```

Shown as a modal overlay (`Clear` + centered Block), not just a footer
strip: tool name plus the command string when the args carry one, else
pretty-printed args JSON. Esc denies the prompt (never auto-allows). Keys
are ignored for ~200 ms after paint so a stray key cannot approve.

Default Go mowi is ask when capabilities are on. This crate should
`perm.set ask` unless `--auto`.

Trust: `mow trust` / `mowi trust` out of band. No marker in the workspace.

## Peers

`acp_delegate` is Engine-only. UI:

- activity: “delegating · <agent>”
- live peer text: collapsed one-liner; `ctrl+p` expands the buffer overlay
- never weld peer chunks onto the host answer
- `harness.delegate.usage` → header `⇄` chip

External peers often omit usage; chip stays host-only then.

## Diffs (flashdiff recipe)

Dark sunk band + theme accent as text. Mocha: add `#a6e3a1` on ~`#334138`,
del `#f38ba8` on ~`#4d3240`. Syntax highlight **context only**. Word-diff:
shared tokens stay full accent; changed tokens invert. Gutter unbanded,
tinted numbers. See mow `packs/mowi/styles.go` / `diff_*.go`.

Diff entries render as a review card: a `─ <file>` rule parsed from the `+++`
header, then full-width bands. Add/del rows are padded to the transcript width
so the wash is a rectangle; the sign column uses `+` / `−` (U+2212) in accent;
`@@` hunks and `---` / `+++` headers stay muted meta; context rows carry no
wash. A `-` row followed by a `+` row gets its changed span inverted as a chip
when the rows share enough affix to make a word-level edit meaningful.

## Resume / scroll

`--session` / `--continue` → `transcript` seed. Virtualize long history.
Follow-bottom until the user scrolls up; rebuild visible window on scroll
(Go bug: placeholders stayed blank). Do not GC unreadied seed.

Transcript scroll is **PgUp / PgDn** (plus wheel events when delivered). **↑ / ↓** browse prompt history. Those keys never recall or
rewrite the composer. Last-prompt recall is `/edit` only. `ctrl+u` / `ctrl+d`
are unbound (they do not scroll and they do not type). The client captures mouse wheel events for transcript-only scrolling. Hold Shift while dragging for terminal-native selection/copy.

## Theme / a11y

- Default theme name: catppuccin-mocha; selectable names are
  catppuccin-mocha, catppuccin-latte, gruvbox-dark, and monokai
- Select with `--theme NAME` or `MOW_THEME=NAME`; unknown names list all
  available themes
- `NO_COLOR=1` — glyphs still distinct (◇ ⚙ ✕ ▲)
- `MOW_NO_ANIM=1` — still spinner; elapsed still ticks
- Native terminal selection/copy (no mouse capture; scroll with ↑↓ / pgup/pgdn)
- Not screen-reader complete; keyboard-complete is required

## Config (Go: `extensions.tui`)

welcome, welcome_message, prompt glyph, theme.name / colors, keys.

v1: env + flags only is OK. Do not require a second config file.

## Files in Go mowi (map for ports)

| File | Role |
|---|---|
| `tui.go` | model, Engine hooks, seedTranscript |
| `tui_update.go` | keys, mouse, overlays |
| `tui_chrome.go` | header, layout, help |
| `tui_transcript.go` | entries, applyVP |
| `tui_stream.go` | live answer + peer buffers |
| `tui_perm.go` | ask strip |
| `tui_commands.go` | `/sessions`, `/status`, … |
| `packslash.go` | slash.Lookup |
| `peer_live.go` | peer collapse |
| `virtual.go` | off-screen placeholders |
| `diff_*.go` | review cards |
| `styles.go` | palette, flashdiff wash |
| `markdown_render.go` | glamour |
| `cmd/mowi/main.go` | flags + pack subcommands |

This crate: `rpc` client + `app` (ratatui) + `render`. No `cmd` pack
dispatch — operators use `mow review` / `mow acp` on the mow binary.

## Out of scope for the Rust binary

- `mowi acp` / `mowi review` as subcommands (call `mow`)
- Implementing ACP
- Embedding libmow via FFI
