# Architecture

## Rule

**mow is headless. The UI is a client. ACP is owned by mow.**

mowi never:

- constructs an Engine
- registers tools or slash commands
- launches Cursor / Gemini / `mow acp` as a peer
- implements review/sec (it *invokes* them via `slash`)

mowi does:

- paint a document (header, transcript, input, overlays)
- send `prompt` / `cancel` / `steer` / `slash` / `perm.set` / `perm.decide`
- render `event` and `perm.ask` notifications
- probe Git branch/dirty locally from the RPC workspace (startup + debounce
  after mutating tools / turn end; never per frame; hide outside a worktree)
- decode optional `extra_roots` when `status` / `session` send them — never

## Processes

```
operator
   │
   ├─ mowi (this crate)          Ratatui, keys, theme, markdown, diffs
   │      │ stdio JSON-lines
   │      ▼
   └─ mow rpc [engine flags]     Engine + linked packs (review, goal, acp, …)
              │
              ├─ LLM HTTP
              ├─ FS tools (jailed)
              └─ acp_delegate ──▶ external / native ACP peers
```

Default: mowi **spawns** `mow` (or `$MOW_BIN`) with `rpc` plus the same engine
flags the user passed. Alternative: attach to an already-running `mow rpc` on
stdio / a socket later — not required for v1.

One Engine per process. In-app session switch is out (by design):
resume with `--session` / `--continue` on the next launch.

## Why RPC, not ACP, for the UI

| | `mow rpc` | ACP |
|---|---|---|
| Role | Native host protocol | Foreign agents / editors |
| Perm | `perm.ask` + y/n/always with tool args | Peer-defined `session/request_permission` |
| Slash | `/review`, `/sec` on **this** Engine | Not a thing |
| Usage | `prompt.usage` + `EventDelegateUsage` | Often omitted by external peers |
| Who owns peers | Engine | — |

If the UI spoke only ACP to `mow acp`, you would lose ask-mode previews,
exclusive slash, and a reliable usage chip. ACP stays how **mow** talks to
other products.

## Protocol version

`version.rpc` is the compatibility epoch. Mowi requires exact epoch `"1"`.
Additive methods do not bump the epoch — feature-detect them from
`methods` / `control_methods` / `features`. A future incompatible wire
contract uses a new epoch; refuse to start against any other value.

See [protocol.md](protocol.md).

## Trust and flags

Workspace trust (`mow trust`) stays out-of-band. mowi may shell out to
`mow trust` or document it; it does not invent a second trust store.

`--allow-write` / `--allow-shell` / `--ask` / `--auto` / `--extra-root` are
**engine flags** passed through to `mow rpc`. `--extra-root` is repeatable
and uses mow's spec: `PATH`, `PATH:ro`, or explicit `PATH:rw`. `--ask` /
`--auto` are forwarded only when present on the CLI. After connect,
`perm.set` mirrors the resolved mode (CLI > `$MOW_PERMISSION_MODE` >
`extensions.mowi` > ask) so the UI and Engine agree.

UI config is `extensions.mowi` via additive `extension.config`
`{name:"mowi"}` when advertised. mowi never opens the host YAML itself.

## Packs

Slash commands exist only if the **mow binary** blank-imported the pack.
`slash.list` is the source of truth for pack-discovered names (`/review`,
`/sec`, `/goal`, …). This crate must not hard-code them or infer a stock
build from an empty `version.methods` list. Help, completion, and
behavior are gated by advertised `methods` / `control_methods` /
`features` plus the cached `slash.list`.

## Rust client properties

Same baseline ([baseline.md](baseline.md)), deliberately better in:

- **Process isolation** — UI is not the Engine. Spawn-kill still cancels.
- **Layout** — Ratatui widgets/constraints instead of hand-rolled lipgloss:
  constraint layout for the frame, `Scrollbar` on the transcript, `Clear` +
  centered Block for help / sessions / peer / permission overlays.
- **Testing** — ratatui `TestBackend` + protocol fixtures for logic, with the
  deterministic `scripts/smoke-tui.sh` PTY smoke when `shell-use` is available.
- **Theme** — mocha default and flashdiff-style diffs as a palette module,
  not Charm AdaptiveColor.
- **No blank-import tricks in the UI** — one crate, one binary.

Do **not** “improve” by putting peer management or review ensemble in the UI.
