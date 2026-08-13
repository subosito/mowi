# UI review — the snapshot tool

`draw` is pure over `App`, so any UI state can be painted into a
`TestBackend` without a live Engine. `mowi snapshot` exposes that as a design
tool: it prints scripted frames to stdout as truecolor ANSI.

```bash
cargo run -- snapshot                              # every scene, 100x30
cargo run -- snapshot --scene permission --width 40 --height 18
NO_COLOR=1 cargo run -- snapshot --scene help      # monochrome frame
```

Scenes live in `src/snapshot.rs`: `chat`, `busy`, `diff`, `welcome`, `help`,
`permission`, `narrow`. Add one whenever a state is hard to reach by hand.

The tool honours `NO_COLOR` through the same `Theme::detect()` the client
uses, so the monochrome frame it prints is the frame users get.

## What to look at when reviewing a change

Re-render at **several widths**, not just your own terminal. Most layout bugs
only exist at the edges:

```bash
for w in 40 48 60 80 100 120; do cargo run -q -- snapshot --scene chat --width $w --height 14; done
```

`MIN_WIDTH` (40) is the narrowest frame the client will paint at all; below
that it shows a "terminal too small" card instead of a broken one.

## Layout rules the frame is built on

**Header — safety outranks vanity.** Capability (`read-only` / `write+shell`)
and ask/auto never drop. Everything else is peeled off the front of
`vanity_chips` as columns run out, ordered by what you can most afford to
lose: tokens → session id → workspace → effort → model. The context gauge is
suppressed below `GAUGE_MIN_COLS` entirely — knowing *which* model you are
talking to outranks knowing how full its window is.

**One live clock.** The activity band above the transcript owns the spinner,
elapsed time and typing pulse. The footer carries only `busy` / `idle`. Two
clocks tick out of step and both stop being believed.

**The footer is a status bar.** State flushed left, key hints flushed right,
hints degrade through progressively shorter variants and drop before the
state does. `ctx%` appears only past `CTX_FOOTER_PCT` — a number that is
always on screen stops being read.

**Modals sit on the document.** Overlays are drawn into the transcript
region, never the full frame, so the composer and status bar stay usable
underneath. `draw_scrim` dims what is behind them (DIM attribute under
NO_COLOR), but never the header chips or the footer decision bar — those are
what the operator reads to decide.

**Consent is the highest-stakes surface.** `decision_line` guarantees all
three of y/a/n survive at every supported width: labels degrade
(`allow once` → `allow` → bare keycap) and the tool name is dropped, but a
decision key is never truncated away. A frame where "allow" is reachable and
"deny" is off screen is a safety bug, not a layout bug. This is pinned by
`deny_survives_every_supported_width`.

**Columns, not characters.** `wrap_cols`, `wrap_styled_line`,
`input_cursor_pos` and every band pad measure with `unicode_width`. CJK and
emoji are two cells; counting `chars()` shears padded backgrounds and drifts
the caret. The composer text and the caret are both derived from
`prompt_layout`, so they cannot disagree about where a row starts.

**Estimates must never under-report.** `visible_transcript_lines` slices the
document using `estimated_entry_lines`, and the scrollbar derives its extent
from the same numbers. If an entry paints taller than it claims, the window
slides and whatever the operator was reading — usually their own prompt — is
pushed off the top of the pane. So estimates count *wrapped* rows, not
logical lines, and `estimated_height_matches_painted_height` pins it.

**Tool rows are labels, not transcripts of the command.** `tool_label` emits
`verb · argument`, collapsing a chained shell blob
(`bash echo ---; cat …; ls -la`) to its first command plus `(+n more)`. This
is the Go `label.go` rule: never mid-string-truncate a shell blob into noise.
The full command is in the engine log; the transcript exists to show *that*
something ran. Pinned by `tool_labels_collapse_shell_chains` and the
`shellblob` snapshot scene.

## Colour

Widgets never name raw colours — they ask `Theme` for a semantic role
(`header`, `badge(Tone::Warn)`, `user_rail`). The palette is Catppuccin
Mocha; a flavour swap should be one table in `theme.rs`.

Every role must degrade under `NO_COLOR` to modifiers only. This is enforced
by `no_color_theme_never_emits_color` — if you add a role, add it there.
