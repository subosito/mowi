# Host protocol (`mow rpc` compatibility epoch 1)

Line-delimited JSON. One object per line. Requests may omit `"jsonrpc":"2.0"`.
Responses and notifications always include it.

The handshake reports three separate identities:

- `rpc` is the wire compatibility epoch. Mowi requires epoch `1`. Additive
  methods do not change it and must be feature-detected from `methods`. A future
  incompatible wire contract uses a new epoch.
- `version` is the mow release/build version, for diagnostics and optional
  human-readable compatibility messages—not protocol gating.
- `jsonrpc` is the JSON-RPC envelope version (`2.0`), not mow's protocol.

Mowi should therefore gate correctness on `rpc` plus advertised methods, not on
a minimum mow release. This supports backports and custom mow builds without
pretending semantic-version ordering describes their capabilities.

Spawn:

```bash
mow rpc [--session ID] [--continue] [--model ID] [--allow-write] [--allow-shell] \
        [--extra-root PATH | PATH:ro | PATH:rw] …
```

`--extra-root` is repeatable. Specs match mow: unsuffixed / `:rw` are
read-write jail roots; `:ro` is read-only.

Child stdin = requests. Child stdout = responses + notifications.
Stderr is Engine logs — do not parse it.

## Handshake

1. `{"id":1,"method":"version"}` → `rpc` must be `"1"`. This is the compatibility epoch, not the mow release version.
   `methods`, `control_methods`, and `features` gate Help / completion /
   startup calls. An empty `methods` list means "not advertised" — do not
   infer a stock build. If `methods` is empty, try `capabilities` once.
2. `{"id":2,"method":"session"}` → workspace, model, session_id.
3. `{"id":3,"method":"status"}` → busy, allow_write, allow_shell, ask_mode.
4. If `extension.config` is advertised: `{name:"mowi"}` for
   `extensions.mowi`. Absent method, empty section, or a failed call
   means built-in defaults.
5. If the UI wants ask mode: `perm.set` `{mode:"ask"}` (default server is
   fail-open auto). Mode comes from CLI `--ask`/`--auto`, then
   `$MOW_PERMISSION_MODE`, then `extensions.mowi.permission_mode`, then
   ask.
6. `transcript` to seed history on `--session` / `--continue` (messages may include additive RFC 3339 `ts`).
7. `slash.list` for `/help` — only when advertised. Pack names (`/goal`,
   `/review`, `/sec`, …) stay dynamic from that list.

## Methods

| Method | Control? | Params | Result |
|---|---|---|---|
| `prompt` | no (worker) | `{text, ephemeral?}` | `{text, session_id, run_id, stop_reason, usage, ephemeral, attached[]}` |
| `slash` | no (worker) | `{name, args[], color?}` | `{title, body, error?}` |
| `cancel` | yes | — | `{ok}` |
| `status` | yes | — | see below |
| `session` / `session_id` | yes | — | `{session_id, workspace, model, wire}` |
| `sessions` | yes | — | `{sessions:[{id, updated, preview}]}` |
| `transcript` | yes | — | `{messages:[{role, content}]}` |
| `steer` | yes | `{text}` | `{ok}` |
| `slash.list` | yes | — | `{commands:[{name, summary, exclusive, aliases}]}` |
| `perm.set` | yes | `{mode:"ask"\|"auto"}` | `{ok, ask_mode}` |
| `perm.decide` | yes | `{id, decision}` | `{ok}` |
| `model.list` | yes | — | `{models:[{id,current,wire?}], current}` |
| `model.set` | yes | `{id}` | `{ok, model}` |
| `effort.list` | yes | — | `{efforts:[{id,current}], current, default}` |
| `effort.set` | yes | `{id}` | `{ok, effort}` |
| `version` | yes | — | `{name, version, rpc, package, methods?, control_methods?, features?}` |
| `capabilities` | yes | — | same surface as `version` when the handshake omitted `methods` |
| `extension.config` | yes | `{name}` | `extensions.<name>` object (see below) |
| `ping` | yes | — | `"pong"` |

Control methods are answered concurrently with an in-flight `prompt`.
**`transcript` can return before the current turn is appended.** After
`prompt` completes, call `transcript` again if you need the stored turns.
Messages are `{role, content}` only — no per-message timestamp. The client
stamps prompts it records locally and leaves resumed history untimed.

`prompt.ephemeral=true` runs an aside against current context without persisting the exchange (`/btw`). Prompt text may contain `@path` references; the RPC host resolves them through the Engine path jail (workspace + extra roots), ignores denied/missing/directory references, deduplicates them, and caps each attachment at 100,000 bytes before appending it for the model. The UI continues to display the original prompt.

`prompt` / `slash` share a worker queue (depth 4). Overflow → error, retry.

The linked `goal` pack registers `/goal` in the same slash registry as
`/review` and `/sec`; it therefore appears in `slash.list` and is invoked with
the generic `slash` method. `/goal` is exclusive and uses the live session
Engine.

Caps: prompt/steer text 512k runes; event deltas 8k; transcript content 32k
per message; stdin line 1 MiB.

### `extension.config`

Additive. Feature-detect from `version` / `capabilities` `methods`.
Params: `{name:"mowi"}`. Result is the decoded `extensions.mowi` section
(or `{config:{…}}` / `{mowi:{…}}`). Unknown fields are ignored.

| Field | Type | Meaning |
|---|---|---|
| `permission_mode` | `"ask"` \| `"auto"` | Initial `perm.set` when CLI/env did not set a mode. |
| `theme` | string | Full theme identifier (`catppuccin-mocha`, `catppuccin-latte`, `gruvbox-dark`, `monokai`). |
| `welcome` | bool | Fresh-session splash. Default on. |
| `welcome_message` | string | Splash tagline; blank keeps the built-in line. |
| `prompt` | string | Composer glyph; always stored with a trailing space. |

Precedence: CLI (`--ask` / `--auto` / `--theme`) > env
(`$MOW_PERMISSION_MODE` / `$MOW_THEME`) > this section > built-in
defaults (`ask`, `catppuccin-mocha`, welcome on, `❯`). An empty
`methods` list means the method is not advertised — do not probe.

### `status`

`busy`, `run_id`, `session_id`, `workspace`, `model`, `wire`,
`allow_write`, `allow_shell`, `ask_mode`, `pending_perm`.

Optional chrome fields (not emitted by current `mow rpc`; the client
decodes them when present — see [Host chrome](#host-chrome)):
`extra_roots` / `extra_root_count`. The same keys are accepted on
`session`. RPC `git` metadata is ignored; the client probes the
workspace itself.

### `slash`

`name` is the token without a required leading slash (`review` or `/review`).
`args:["help"]` (also `-h`, `--help`, `?`) returns usage without `Run`.

`exclusive && status.busy` → JSON-RPC error (not an envelope): refuse like
the maintained TUI.

`Run` failures (empty scope, bad flags) are **`{title, body, error}`** with
a success `id` — paint as an error entry, not a crashed socket.

### `perm.*`

Default: tools run without asking.

After `perm.set` ask, `write` / `edit` / `bash` emit:

```json
{"jsonrpc":"2.0","method":"perm.ask","params":{"id":"perm-1","name":"write","args":{},"tool_call_id":"…"}}
```

Reply:

```json
{"id":9,"method":"perm.decide","params":{"id":"perm-1","decision":"allow"}}
```

`decision`: `allow` | `deny` | `always` (`always` = this tool, this session).
Unknown `id` → invalid request. Cancelled turn while waiting aborts the run.

Read tools never ask.

## Notifications (`method`, no `id`)

| Method | When |
|---|---|
| `event` | Engine bus (`AddOnEvent`) |
| `perm.ask` | Power tool blocked on the UI |

### Event types the UI should handle

Exact strings live in mow `Event*` consts (`mow.go`). Handle at least:

| Type (typical) | UI |
|---|---|
| token / `loop.token` | append live answer (`delta`) in the transcript while busy |
| reasoning / thinking | activity / collapsible thought (body never painted) |
| `loop.turn` | fallback live answer when token deltas were omitted |
| `harness.tool.start` / `harness.tool.end` | status-bar verb + bounded in-transcript progress (`args` → path/command; `result` diffs for write/edit; `error`/`denied` immediately). Folds into the turn's tool group at `run.end` |
| `run.start` / `run.end` | busy; usage on end |
| `harness.delegate.chunk` | peer live buffer (`agent`, `delta`) — **not** host answer |
| `harness.delegate.progress` | peer phase (`thought`, `tool`, `prompt`) |
| `harness.delegate.usage` | add to peer token chip |
| `graph.goal.start` / `step` / `done` / `fail` | status-bar Goal state: short id + `step/max` |
| `graph.goal.partial` / `blocked` | status-bar Goal state: `blocked`, or step/max while partial |
| compact | refresh ctx% |
| lsp diagnostics | optional diagnostics line |

Dump one `prompt` in tests or read `mow.go` — do not guess ACP method names.

## IDs

Monotonic integer `id` per request is enough. Notifications have no `id`.
Correlate `perm.decide` with `perm.ask`’s `params.id` (string `perm-N`).

## Errors

JSON-RPC: `-32700` parse, `-32600` invalid, `-32601` unknown method,
`-32603` internal (prompt transport / Engine error). Prompt errors may
include `data: {text, session_id, run_id, stop_reason}`.

### `graph.goal.*` payload

Confirmed host event types: `graph.goal.start`, `graph.goal.step`,
`graph.goal.done`, `graph.goal.fail`, `graph.goal.partial`,
`graph.goal.blocked`. The frozen payload is:

```json
{
  "type": "graph.goal.step",
  "goal": {
    "id": "fix-bugs",
    "status": "running",
    "step": 2,
    "max_steps": 10
  }
}
```

The client paints `goal {short-id} {step}/{max}` while running/partial,
or `blocked` / `failed` / `done`. Event type wins for terminal and
blocked so a stale `status` cannot leave a running chip. `done` /
`failed` clear on the next user prompt.

## Host chrome

### Git (client-owned)

- Probe on startup and after a mutating tool (`write` / `edit` /
  `bash` / `apply_patch` / `str_replace`) or turn end.
- Debounce (~800 ms). Never probe on a paint frame.
- Cap each child at ~250 ms; a hung index must not stall the UI.
- Hide the chip outside a Git worktree, or when `workspace` is empty.
- Paint `main` or `main*` when dirty.

### `extra_roots` (array, optional)

```json
"extra_roots": [
  { "path": "/opt/shared", "read_only": true },
  { "path": "/data", "read_only": false }
]
```

| Field | Type | Meaning |
|---|---|---|
| `path` | string | Jail root beyond the workspace, after Engine policy merge (CLI `--extra-root`, user config, workspace profile). |
| `read_only` | bool | Defaults to `false`. |

A string element `"/path"` is accepted as a read-write root. Alternate:
`extra_root_count` (number) when the host only wants to expose the
count. The chip is `+N roots` (`+1 root` in the singular) and is
hidden when N is 0 or the field is absent.

Absent keys leave previously decoded extra-root chips in place so a
current mow `status` cannot wipe a `session` that did send them. An
explicit empty `extra_roots` array clears the chip.

## What is not on the wire (yet)

- `extra_roots` on `status` and `session` — client decodes the shapes
  above; host does not emit them yet. Git is client-owned.
- Model picker catalog — v1 can omit
- Keys — UI-local; not configurable through `extensions.mowi` yet
- Theme / permission / welcome — `extension.config` `{name:"mowi"}` when
  advertised; otherwise CLI, env, and built-in defaults
- Binary diffs — events carry text; UI pretty-prints
- Spawning peers — Engine only
