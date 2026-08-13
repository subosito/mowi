# mowi (Rust)

A Ratatui terminal UI for [mow](https://github.com/subosito/mow). This
repository is a **client**. The agent loop, tools, path jail, sessions, ACP
peers, and review/sec all live in the headless `mow` process.

```
[mowi — Ratatui]
        │  JSON-lines  (mow rpc, protocol v3)
        ▼
[mow Engine] ── acp_delegate ──▶ Cursor / Gemini / mow acp
        └── /review /sec, sessions, perm policy
```

Go mowi (`mow/packs/mowi`) is the in-process reference host. This crate aims
at the **same product surface** over a wire, not a Charm port.

| Doc | Read when |
|-----|-----------|
| [architecture.md](architecture.md) | Process split, what mow owns vs the UI |
| [protocol.md](protocol.md) | `mow rpc` v3 methods, events, perm.ask |
| [baseline.md](baseline.md) | Feature inventory of Go mowi (the bar) |
| [roadmap.md](roadmap.md) | Phases from hello-RPC to parity |

## Run (once the crate exists)

```bash
# sibling checkout: mow built as ../mow/bin/mow
devenv shell -- cargo run -- --help
# mowi spawns: $MOW_BIN rpc [engine flags]
```

mowi should spawn `mow rpc` by default (same flags as today’s Go `mowi` CLI:
`--session`, `--continue`, `--allow-write`, `--allow-shell`, `--ask` /
`--auto`). It must not embed Engine and must not spawn ACP peers.

## Public samples

Use current public model ids in docs and examples (`gpt-5-mini`,
`claude-sonnet-4`, `gemini-2.5-flash`, `deepseek-chat`). No private fleet
names.
