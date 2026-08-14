# mowi

Ratatui client for the [mow](https://github.com/subosito/mow) harness.
The Engine stays headless (`mow rpc`). This UI does not embed mow and
does not manage ACP peers.

**Start here:** [docs/README.md](docs/README.md)

| Doc | |
|-----|---|
| [docs/architecture.md](docs/architecture.md) | Process split |
| [docs/protocol.md](docs/protocol.md) | `mow rpc` v3 |
| [docs/baseline.md](docs/baseline.md) | Go mowi feature bar |
| [docs/roadmap.md](docs/roadmap.md) | Phases |

```bash
devenv shell -- cargo test
devenv shell -- cargo run -- --help
```

Requires `mow` on `PATH` (or `$MOW_BIN`) built from the sibling `mow`
repo with `ext/rpc` linked. Protocol `version.rpc` must be `"3"`.

Themes are selected with full names:

```bash
mowi --theme catppuccin-mocha
MOW_THEME=gruvbox-dark mowi
```

Available themes are `catppuccin-mocha`, `catppuccin-latte`, `gruvbox-dark`,
and `monokai`. `NO_COLOR=1` disables palette colors while retaining semantic
text modifiers.

## PTY smoke

With [Microsoft shell-use](https://github.com/microsoft/shell-use) installed:

```bash
scripts/smoke-tui.sh
```

The smoke uses a deterministic JSON-RPC fixture; it requires no model credentials.
