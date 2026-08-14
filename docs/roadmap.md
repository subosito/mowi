# Roadmap

Historical implementation phases for the maintained Rust UI over additive
`mow rpc` (compatibility epoch 1).

## Phase 0 — crate + spawn ✓

- Cargo bin `mowi`
- Spawn `$MOW_BIN` or `mow` with `rpc` + passthrough engine flags
- JSON-lines client: id allocator, request/response, notification split
- Handshake: `version` (require `rpc=1`), `session`, `status`
- Ratatui: header (model/workspace), transcript viewport, input, status line
- Enter → `prompt`; Esc → `cancel`; quit
- Render `event` token deltas as live assistant text
- Tests: client codec against fixture lines (no live mow required)

## Phase 1 — host protocol in the UI ✓

- `transcript` seed on `--session` / `--continue`
- `perm.set` + `perm.ask` strip (y/n/a)
- `steer` while busy
- `slash.list` + `slash` for `/help`, `/review`, `/sec` when present
- Exclusive slash refused when busy
- `/sessions` from `sessions`
- Token chip from `prompt.usage` + delegate.usage events
- Follow-bottom / scroll (↑↓ / pgup/pgdn)

## Phase 2 — document quality ✓

- Markdown in assistant entries
- Tool lines from tool.start/end
- Flashdiff-style add/del cards
- Peer collapse (one line + expand)
- Activity band verbs
- Theme mocha + NO_COLOR
- Virtualized transcript

## Phase 3 — polish ✓

- Queued prompts while busy
- Wheel transcript scrolling (Shift+drag keeps terminal-native selection)
- `/search`, `/copy`, `/retry`, `/edit`
- Remappable keys through `extensions.mowi` if needed
- Deterministic PTY smoke via `scripts/smoke-tui.sh` when `tui-test` is available

## Phase 4 — product chrome & key parity ✓

Ratatui widgets carry the chrome; the transcript stays the product.

- **Header chips** built as spans: left identity is `mowi` / workspace
  basename / `model (effort)`; after safety, optional extra-root /
  status-bar Goal states, then the token chip (`⇄` peer), then the context size
  (`32k/128k ctx` or `32k ctx`) at the far right. A ` · ` joins safety
  to the first optional chip and is omitted when none remain. Drop
  order: tokens, extra-roots, context, then identity.
  Safety never drops. Git is a cached local workspace probe (startup +
  debounce; never per frame). Extra-roots decode from host
  `status`/`session` only. Goal is driven by `graph.goal.*` and clears
  after completion on the next prompt. Help / completion hide
  unadvertised optional commands instead of inferring a stock build. Session id is never a header or
  status-bar chip; the help overlay titles the full id. `status` seeds
  capability chips at handshake.
- **Min size 40×10** — below that a centered `Clear` + bordered warning Block
  instead of a broken frame.
- **Welcome splash** for a fresh session (no transcript seed); any key
  dismisses it.
- **Status bar** owns the live clock while busy (spinner + elapsed + verb;
  typing pulse while tokens land). Idle is `● idle`. `MOW_NO_ANIM=1` pins a
  static `●` and the clock still ticks. There is no separate activity band.
- **Transcript** gets `Paragraph` + a `Scrollbar` bound to the scroll state,
  and diffs render as a review card with a `─ <file>` title parsed from `+++`.
- **Input** is a multi-line Paragraph that grows 1–6 rows; `ctrl+j` inserts a
  newline, ↑/↓ scroll the transcript without rewriting the prompt, `/edit`
  loads the last user prompt, `ctrl+l` clears the painted transcript (Engine
  history is untouched).
- **Overlays** (`Clear` + centered Block): help (`List` of local keys plus
  `slash.list` rows), `/sessions` (id / updated / preview + `mowi --session
  <id>` hint), peer buffer, and permissions (tool name + command string or
  pretty-printed args, `y`/`n`/`a`). The permission overlay ignores keys for
  ~200 ms after paint so a stray keystroke cannot approve a power tool.
- **Peers** collapse to `→ agent · …`; `ctrl+p` expands the last agent's buffer
  in an overlay. Peer chunks still never weld onto the host answer.
- **shift+tab** flips ask/auto locally and pushes `perm.set`.
- Theme gained `chrome` / `chip` / `warn` / `accent` roles; `NO_COLOR` keeps
  the glyphs distinct.

Still deliberately out: a remappable keymap file.

## Phase 5 — production chrome & flashdiff bands ✓

- **`/quit` `/exit` `/q` are local.** A `slash_route` name router runs before
  the RPC fallback, so a UI command can never be forwarded to the host as an
  unknown slash. Quit cancels an in-flight turn first, like ctrl+c.
- **Flashdiff bands** — add/del rows pad to the transcript width so the wash is
  a rectangle, with `+` / `−` (U+2212) signs in accent, muted `@@` and file
  headers, unwashed context rows, and an inverted word chip on the changed span
  of a `-`/`+` pair. `NO_COLOR` keeps signs and structure without RGB.
- **Chrome** — header chips close with a hairline rule; the transcript reserves
  the scrollbar column so bands never run under the thumb; the input sits on
  the document ground with no box, with a `❯` glyph (`>` when colour
  is off); the footer is its own status bar on the terminal default with a
  top rule and goes
  quiet while the permission overlay owns the decision.
- **Permission overlay** — the tool name titles the block and `y/n/a` sits on
  the title-right, with args as a wrapped Paragraph.

## Non-goals

- In-app Engine
- UI-owned ACP peers
- Ensemble `--reviewer` inside `/sec`
- Replacing `mow` CLI

## Done when (v0)

```
cargo test
cargo run -- --help
# with mow on PATH and credentials:
cargo run --
# type hi, see a streamed answer, quit cleanly
```
