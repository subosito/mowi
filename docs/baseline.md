# Baseline — what Go mowi already is

Source of truth: `mow/packs/mowi`. This crate should reach **the same
operator experience**, not a subset that forgets permissions or resume.

## Screen (document, not chat log)

| Region | Behavior |
|---|---|
| Header | Workspace, model, session, safety chips (write/shell, ask/auto). Narrow terminals drop vanity first; safety never drops. Token chip = host + peer (`⇄`). |
| Transcript | User blocks (soft fill), assistant markdown, one compact tool line per turn. Edits = inline review cards (−/+). |
| Activity band | Only while busy: spinner, verb (“searching · grep · loop.go”), elapsed. |
| Input | Enter sends; busy queues. `/steer` redirects the running turn. |
| Welcome | Splash, dismisses on any key. |
| Min size | 40×10; below that a size warning, not a broken frame. |

## Input & keys (defaults)

| Key | Action |
|---|---|
| Enter | send (queue if busy) |
| ctrl+j | newline |
| ctrl+u / ctrl+d | scroll transcript (wheel too) |
| Esc | cancel turn / dismiss overlay / deny? (cancel first) |
| ctrl+l | clear transcript (UI-local; Engine history remains) |
| shift+tab | ask ↔ auto (`perm.set`) |
| ctrl+s | select mode — release mouse for native copy |
| ctrl+/ or `?` on empty | help |
| ctrl+c | quit (cancel first if busy) |
| Arrow-up on empty | edit last prompt |

All bindings remappable later (`extensions.tui.keys` in Go). v1 can hard-code
defaults.

## Commands (typed)

Host-side (this crate, or slash if registered):

| Command | Notes |
|---|---|
| `/steer <text>` | RPC `steer` while busy |
| `/status` | RPC `status` + last usage; show peer share |
| `/sessions` | RPC `sessions` (list + how to resume; no in-app switch) |
| `/search` | UI-local find in painted transcript |
| `/copy` `/retry` `/edit` | UI-local / re-`prompt` |
| `/model` `/effort` | later; Engine/CLI today |
| `/review` `/sec` `/goal` … | RPC `slash` — only if `slash.list` has them |

Exclusive slash: refuse while `status.busy`.

`/review` and `/sec` use the **session model**. They do not start a
`--reviewer` ensemble. Ensemble is `mow review` CLI.

## Permissions

Power tools (`write`, `edit`, `bash`) when ask mode:

```
y allow · n deny · a always (this session)
```

Preview: command string or diff from `perm.ask` args. Esc cancels the
prompt (does not auto-allow). Short ignore window after paint so a stray
key cannot approve.

Default Go mowi is ask when capabilities are on. This crate should
`perm.set ask` unless `--auto`.

Trust: `mow trust` / `mowi trust` out of band. No marker in the workspace.

## Peers

`acp_delegate` is Engine-only. UI:

- activity: “delegating · <agent>”
- live peer text: collapsed one-liner; expand later (`ctrl+p` in Go)
- never weld peer chunks onto the host answer
- `harness.delegate.usage` → header `⇄` chip

External peers often omit usage; chip stays host-only then.

## Diffs (flashdiff recipe)

Dark sunk band + theme accent as text. Mocha: add `#a6e3a1` on ~`#334138`,
del `#f38ba8` on ~`#4d3240`. Syntax highlight **context only**. Word-diff:
shared tokens stay full accent; changed tokens invert. Gutter unbanded,
tinted numbers. See mow `packs/mowi/styles.go` / `diff_*.go`.

v1 can show a raw unified diff; parity needs this recipe.

## Resume / scroll

`--session` / `--continue` → `transcript` seed. Virtualize long history.
Follow-bottom until the user scrolls up; rebuild visible window on scroll
(Go bug: placeholders stayed blank). Do not GC unreadied seed.

ctrl+u is **scroll**, not kill-to-start-of-line.

## Theme / a11y

- Default theme name: catppuccin-mocha (chroma-compatible idea)
- `NO_COLOR=1` — glyphs still distinct (◇ ⚙ ✕ ▲)
- `MOW_NO_ANIM=1` — still spinner; elapsed still ticks
- `MOW_MOUSE=0` / select mode — native selection
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
