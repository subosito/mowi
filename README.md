# mowi - mow with interface

The terminal interface for the [mow](https://github.com/subosito/mow) harness.
Mowi is built using Ratatui and connects to the headless Engine through
`mow rpc`; it does not embed mow or manage ACP peers itself.

**Start here:** [docs/README.md](docs/README.md)

| Doc | Purpose |
|---|---|
| [docs/architecture.md](docs/architecture.md) | Process split and ownership |
| [docs/protocol.md](docs/protocol.md) | Additive `mow rpc` contract |
| [docs/baseline.md](docs/baseline.md) | Current mowi behavior |
| [docs/ui-review.md](docs/ui-review.md) | Snapshot and responsive-layout review |
| [docs/roadmap.md](docs/roadmap.md) | Historical implementation phases |

```bash
devenv shell -- cargo test
devenv shell -- cargo run -- --help
```

Requires `mow` on `PATH` (or `$MOW_BIN`) with `ext/rpc` linked. Mowi requires
RPC compatibility epoch `1` and discovers additive behavior through
`capabilities` plus `slash.list`. Pair with **mow ≥ 1.0.0-rc.1**. This
crate is also **1.0.0-rc.1** (`Cargo.toml`); tag `v1.0.0-rc.1` to publish.

Version is `package.version` in [`Cargo.toml`](Cargo.toml). Tag `v$(that)`
to run [`.github/workflows/release.yml`](.github/workflows/release.yml)
(linux/darwin amd64+arm64, GitHub Release; `*-rc.*` is a prerelease).

## Themes

Themes use full identifiers:

```bash
mowi --theme catppuccin-mocha
MOW_THEME=gruvbox-dark mowi
```

Available themes are `catppuccin-mocha`, `catppuccin-latte`, `gruvbox-dark`,
and `monokai`. Precedence is `--theme` > `$MOW_THEME` >
`extensions.mowi.theme` > `catppuccin-mocha`. Permission mode is
`--ask`/`--auto` > `$MOW_PERMISSION_MODE` >
`extensions.mowi.permission_mode` > ask. The pack section is fetched
with `extension.config` `{name:"mowi"}` when the host advertises that
method. `NO_COLOR=1` disables palette colors while retaining semantic text
modifiers.

## PTY smoke

With [Microsoft tui-test](https://github.com/microsoft/tui-test) on `PATH`
(included in `devenv shell`):

```bash
scripts/smoke-tui.sh
```

The smoke uses a deterministic JSON-RPC fixture and requires no model
credentials. It checks startup/resume, themes, resize, scrolling and typing,
live assistant/tool progress, immediate write/edit diff cards, and clean exit.
