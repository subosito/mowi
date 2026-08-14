# mowi - mow with interface

Mowi is the terminal interface for [mow](https://github.com/subosito/mow),
built using Ratatui. This repository is a **client**. The agent loop, tools,
path jail, sessions, ACP peers, and review/sec all live in the headless `mow`
process.

```
[mowi — mow with interface]
        │  JSON-lines  (mow rpc, compatibility epoch 1)
        ▼
[mow Engine] ── acp_delegate ──▶ Cursor / Gemini / mow acp
        └── /review /sec, sessions, perm policy
```

The former Go `packs/mowi` host has been retired. This Rust client is the maintained TUI over the additive RPC contract.

| Doc | Read when |
|-----|-----------|
| [architecture.md](architecture.md) | Process split, what mow owns vs the UI |
| [protocol.md](protocol.md) | `mow rpc` compatibility, methods, events, perm.ask |
| [baseline.md](baseline.md) | Current Rust mowi feature and behavior inventory |
| [ui-review.md](ui-review.md) | Snapshot tool + the layout rules to review against |
| [roadmap.md](roadmap.md) | Phases from hello-RPC to parity |

## Run

```bash
# sibling checkout: mow built as ../mow/bin/mow
devenv shell -- cargo run -- --help
# mowi spawns: $MOW_BIN rpc [engine flags]
```

mowi spawns `mow rpc` by default (with supported Engine flags:
`--session`, `--continue`, `--model`, `--effort`, `--allow-write`,
`--allow-shell`, `--ask` / `--auto`, repeatable `--skill NAME` and
`--extra-root PATH` /
`PATH:ro` / `PATH:rw`). Theme and permission mode also read
`$MOW_THEME` / `$MOW_PERMISSION_MODE` and, when advertised,
`extension.config` `{name:"mowi"}` (`extensions.mowi`). It does not
embed the Engine or spawn ACP peers.

## Public samples

Use current public model ids in docs and examples (`gpt-5-mini`,
`claude-sonnet-4`, `gemini-2.5-flash`, `deepseek-chat`). No private fleet
names.
