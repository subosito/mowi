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
`permission`, `tools`, `narrow`. Add one whenever a state is hard to reach by
hand.

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

**Header — identity left, usage right, safety never drops.** The left cluster
is only `mowi`, the workspace *basename*, and `model (effort)` with the
effort word dimmed. Token count and the context gauge sit right-aligned
against the capability / ask chips and peel first (tokens, then gauge) so a
tight row keeps identity. The gauge is not offered below `GAUGE_MIN_COLS`.
Session id is never a header chip. The painted row has a one-column inset on
each side so it shares a vertical rhythm with the composer and footer.

**One live clock.** The activity band above the composer owns the spinner,
elapsed time and typing pulse, each painted in its own role (`spinner` /
`timing` / verb / `typing`) so the row has hierarchy instead of reading as
one muted sentence. The footer names the state (`idle`, or the current verb
while a turn runs) and never repeats the clock. Its enter hint says `queue`
while a turn is running. Two clocks tick out of step and both stop being
believed.

**The footer is a status bar.** State flushed left, key hints flushed right,
hints degrade through progressively shorter variants and drop before the
state does. `ctx%` appears only past `CTX_FOOTER_PCT` — a number that is
always on screen stops being read. The full session id sits with the state
when status and the minimum `?` hint still fit; long hints degrade around
it. It is hidden only when those two cannot take ` · <id>`. The bar owns a
two-row region: a top hairline plus the status text, so the rule never
eats the line.

**Bottom chrome is one hairline.** The composer sits on the document ground
with a horizontal inset and no box. The status line is a separate sunk bar
with its own top rule. Activity stays a one-line transient band immediately
above the composer; it is not folded into the status line.

**Scroll and recall are different keys.** PgUp/PgDn move the transcript and
never rewrite the composer. Arrow-up recalls the last user prompt only when
the input is empty; it does not scroll. `t` always types. `ctrl+u` / `ctrl+d`
are unbound.

**Modals sit on the document.** Overlays are drawn into the transcript
region, never the full frame, so the composer and status bar stay usable
underneath. `draw_scrim` dims what is behind them (DIM attribute under
NO_COLOR), but never the header chips or the footer decision bar — those are
what the operator reads to decide. The welcome card degrades by height so
access and `type to begin` survive at `MIN_WIDTH`×`MIN_HEIGHT`; the help
table sizes its key column to the keys so actions are not sheared at a
normal 80-wide frame. The permission card uses the same overlay ground as
the other modals — urgency lives in the warn border and the decision keys,
not a second fill.

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

**Tool calls group per turn, not per call.** `tool.start`/`tool.end` events
accumulate in `live_tools` while the loop runs (the activity band owns the
live "tool · name" readout); `run.end` (or the turn's end, whichever comes
first — commit is idempotent) folds them into one `Entry::Tools`. A single
call stays the plain one-row entry it always was; two or more collapse to
`⚙ N tool calls · total`. Esc collapses an expanded group. A plain `t` always
types into the composer — it is never a tool-group shortcut. The estimate
counts the collapsed line exactly as painted and, when expanded, header +
every call, so the scrollbar extent never drifts. Pinned by
`tools_estimate_matches_painted_height` and the `tools` snapshot scene.

## Colour

Widgets never name raw colours — they ask `Theme` for a semantic role
(`header`, `badge(Tone::Warn)`, `user_rail`). The palette is Catppuccin
Mocha; a flavour swap should be one table in `theme.rs`.

Every role must degrade under `NO_COLOR` to modifiers only. This is enforced
by `no_color_theme_never_emits_color` — if you add a role, add it there.
