# Baseline — current Rust mowi behavior

Source of truth: the Rust implementation and its snapshot/PTTY tests. Optional host features are advertised by RPC capabilities and `slash.list`.

## Screen (document, not chat log)

| Region | Behavior |
|---|---|
| Header | Left: `mowi`, workspace basename, `model (effort)`. Right: safety (`write`/`shell` combinations, ask/auto), optional `+N roots`, tokens, and far-right context size (`32k/128k ctx` or `32k ctx`). Optional chips use ` · ` separators and disappear cleanly. Drop order: tokens → roots → context → identity; safety never drops. |
| Transcript | User blocks (soft fill), assistant markdown, one compact tool line per call — a turn's calls collapse into a counted row (`⚙ bash ×2 · grep`) that shortens by whole tokens on a narrow pane (Esc collapses an expanded group). While busy, a bounded live progress section shows streaming answer tokens, the current/recent tool (verb + path), and write/edit diffs as review cards; it folds into the tool group at turn end. Edits = inline review cards (−/+). |
| Status bar | Idle: `● idle`. Busy: one stable turn spinner, elapsed, verb, optional Goal progress, concurrent peer count, and queue state. Tool rows in the transcript use their own clock-motion spinner. |
| Input | Sits on the document ground with a horizontal inset and no box. Enter sends; busy queues. `/steer` redirects the running turn. |
| Welcome | Splash, dismisses on any key. Short panes drop the tagline/effort first so access and `type to begin` still fit. |
| Min size | 40×10; below that a size warning, not a broken frame. |

## Input & keys (defaults)

| Key | Action |
|---|---|
| Enter | send (queue if busy) |
| ctrl+j | newline (input grows 1–10 rows) |
| ↑ / ↓ | browse prompt history |
| Esc | dismiss overlay / collapse tools; while busy, press twice within 1.5s to cancel |
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

Key bindings are currently fixed. v1 hard-codes the defaults. Quit is `/quit` / `/exit` / `/q` or ctrl+c — a lone `q` on empty
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

Mowi requests ask mode unless `--auto`, `$MOW_PERMISSION_MODE=auto`, or
`extensions.mowi.permission_mode: auto` wins the precedence stack
(CLI > env > pack > default ask). `--ask` / `--auto` are explicit;
absent flags fall through.

Trust: `mow trust` / `mowi trust` out of band. No marker in the workspace.

## Peers

`acp_delegate` is Engine-only. UI:

- activity: “delegating · <agent>”
- live peer text: collapsed one-liner; `ctrl+p` expands the buffer overlay
- never weld peer chunks onto the host answer
- `harness.delegate.usage` → header `⇄` chip

External peers often omit usage; chip stays host-only then.

## Diffs (flashdiff recipe)

Dark sunk band + theme text as the row body; dedicated add/del accents on
the sign and the inverted word chip (not the semantic ok/error green/red).
Mocha: text `#cdd6f4` on add `#334138` / del `#3c2f34`; signs `#a6e3a1` /
`#f38ba8`. Syntax highlight **context only**. Word-diff: shared tokens stay
on the band; changed tokens invert. Gutter unbanded, tinted numbers.

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
- Select with `--theme NAME` or `MOW_THEME=NAME`; those beat
  `extensions.mowi.theme`. Unknown CLI/env names list all available themes.
- `NO_COLOR=1` — glyphs still distinct (◇ ⚙ ✕ ▲)
- `MOW_NO_ANIM=1` — still spinner; elapsed still ticks
- Mouse wheel is captured for transcript scrolling; hold Shift while dragging for terminal-native selection/copy
- Not screen-reader complete; keyboard-complete is required

## Config (`extensions.mowi`)

Fetched over RPC with `extension.config` `{name:"mowi"}` when the host
advertises the method. The client does not open a second config file.

```yaml
extensions:
  mowi:
    permission_mode: ask          # ask | auto
    theme: catppuccin-mocha       # full identifier
    welcome: true
    welcome_message: hello pack   # optional splash tagline
    prompt: "❯"
```

Precedence: CLI > env (`$MOW_THEME`, `$MOW_PERMISSION_MODE`) >
`extensions.mowi` > built-in defaults. Absent method → defaults after
CLI/env. Keys stay hard-coded.

## Out of scope for the Rust binary

- `mowi acp` / `mowi review` as subcommands (call `mow`)
- Implementing ACP
- Embedding libmow via FFI
