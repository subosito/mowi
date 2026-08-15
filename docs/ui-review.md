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
`permission`, `tools`, `toolgroup`, `narrow`, `header`, `progress`. Add one
whenever a state is hard to reach by hand.

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
effort word dimmed. After safety, optional chips paint as git, extra-root
count, Goal, tokens, then the context size (`32k/128k ctx` or `32k ctx`)
at the far right. Pressure colour still uses the internal percent
(muted / warn ≥75% / error ≥90%); the header does not print `%` or a
fill bar. A ` · ` joins safety to the first optional chip and is omitted
when none remain. Drop order (first to go): tokens, git, extra-roots,
Goal, context, then identity (workspace, effort, model). There is no
minimum-width gate — the compact size chip is offered whenever used
tokens are known and peels when the row overflows.
probe of the RPC workspace (never per frame, hidden outside a worktree).
Extra-roots appear only from host `status`/`session` fields. Goal appears
only from `graph.goal.*` and does not stay after completion once the
operator continues. Session id is never a header chip. The painted row has a
one-column inset on each side so it shares a vertical rhythm with the
composer and footer. Header and status sit on the terminal default
background — they do not paint a second mantle fill over the document
ground.

**One live clock.** The status bar owns the spinner, elapsed time and
typing pulse while a turn runs, each painted in its own role (`spinner` /
`timing` / verb / `typing`) so the row has hierarchy instead of reading as
one muted sentence. Idle is a state light (`● idle`). There is no second
activity band above the composer — two clocks tick out of step and both
stop being believed. The enter hint says `queue` while a turn is running.

**The footer is a status bar.** State flushed left, key hints flushed right,
hints degrade through progressively shorter variants and drop before the
state does. `ctx%` appears only past `CTX_FOOTER_PCT` — a number that is
always on screen stops being read. Session id is never status-bar chrome;
the help overlay titles the full id. The bar owns a two-row region: a top
hairline plus the status text, so the rule never eats the line.

**Bottom chrome is one hairline.** The composer sits on the document ground
with a horizontal inset and no box — no blank pad rows, because those
share the transcript ground and read as a tall empty well against the
status rule. The status line is a separate bar on the terminal default with
its own top rule. The live
clock folds into that bar while a turn runs; it is not a transient band
above the composer.

**Scroll and recall are different keys.** ↑/↓ and PgUp/PgDn move the
transcript and never rewrite the composer. Last-prompt recall is `/edit`
only. `t` always types. `ctrl+u` / `ctrl+d` are unbound. The client does
not capture the mouse; a wheel event, if one arrives, scrolls the
transcript only.

**Modals sit on the document.** Overlays are drawn into the transcript
region, never the full frame, so the composer and status bar stay usable
underneath. `draw_scrim` dims what is behind them (DIM attribute under
NO_COLOR), but never the header chips or the footer decision bar — those are
what the operator reads to decide. The welcome card degrades by height so
access and `type to begin` survive at `MIN_WIDTH`×`MIN_HEIGHT`; the help
card sizes to its rows (capped) so a tall frame is not a pane-filling
empty table, and the key column yields on a narrow pane so actions stay
readable. The full session id sits on the title rail (the title shortens
to `help` when the long name plus the id would collide) so the card does
not grow a header row. At a normal 80-wide frame the key column still
fits the keys so actions are not sheared. The permission card uses the same overlay ground as
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

**User prompts carry an inline clock, not a label.** A prompt this client
recorded is prefixed with a muted UTC `HH:MM` on the first band row — same
row as the text, not a timestamp line above it. Resumed `transcript`
messages have no per-message time on the wire, so they render without a
stamp rather than inventing one. The wrap estimate uses the same display
string as the painter. Pinned by `user_prompt_stamp_is_inline_when_recorded`
and `resumed_user_prompt_has_no_invented_stamp`.

**Tool rows are labels, not transcripts of the command.** `tool_label` emits
`verb · argument`, collapsing a chained shell blob
(`bash echo ---; cat …; ls -la`) to its first command plus `(+n more)`. This
is the Go `label.go` rule: never mid-string-truncate a shell blob into noise.
The full command is in the engine log; the transcript exists to show *that*
something ran. Pinned by `tool_labels_collapse_shell_chains` and the
`shellblob` snapshot scene.

**Tool calls group per turn, not per call.** `harness.tool.start`/`end`
events accumulate in `live_tools` while the loop runs. The status bar still
owns the phase verb; the transcript also paints a bounded live progress
section (tally + last few rows + the latest write/edit diff cards from
`result`). `run.end` (or the turn's end — fold is idempotent) commits the
group and persists those diffs once. A single call stays the plain one-row
entry it always was; two or more collapse to `⚙ bash ×2 · grep · total`.
The counts follow first-seen verb order from `tool_label` and drop to
`bash ×2 · grep · …` then verbs-only when the pane is too narrow — never a
mid-token cut. Esc collapses an expanded group. A plain `t` always types
into the composer — it is never a tool-group shortcut. The estimate counts
the collapsed line exactly as painted and, when expanded, header + every
call, so the scrollbar extent never drifts. Pinned by `tool_group_summary_*`,
`live_progress_paints_tokens_tools_and_diffs_then_folds_once`,
`tools_estimate_matches_painted_height`, and the `tools` / `toolgroup` /
`progress` snapshot scenes.

## Colour

Widgets never name raw colours — they ask `Theme` for a semantic role
(`header`, `badge(Tone::Warn)`, `user_rail`). The palette is Catppuccin
Mocha; a flavour swap should be one table in `theme.rs`. Diffs use a
dedicated `DiffPalette` (sage/rose washes, theme-text body, accent on the
sign and word chip) so they do not share ok/error green/red. Historical
user text is theme `text` on the user band. Identity is the surface0
band plus the lavender rail — not a peach paragraph. Peach is reserved
for `ctx_warn`. Inline `` `code` `` is theme `text` on surface0 (the chip
is the ground). Header and footer grounds
are `Color::Reset` (the terminal default).

Every role must degrade under `NO_COLOR` to modifiers only. This is enforced
by `no_color_theme_never_emits_color` — if you add a role, add it there.
