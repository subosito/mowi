# Roadmap

Ship a **running** UI against `mow rpc` v3, then climb toward Go mowi
parity. Do not block v0 on diffs or `/model`.

## Phase 0 — crate + spawn ✓

- Cargo bin `mowi`
- Spawn `$MOW_BIN` or `mow` with `rpc` + passthrough engine flags
- JSON-lines client: id allocator, request/response, notification split
- Handshake: `version` (require `rpc=3`), `session`, `status`
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
- Follow-bottom / scroll (ctrl+u/d)

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
- Select-mode mouse
- `/search`, `/copy`, `/retry`, `/edit`
- Remappable keys / `extensions.tui` if we choose to read mow config
- PTY cell smoke (optional)

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
