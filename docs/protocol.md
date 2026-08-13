# Host protocol (`mow rpc` v3)

Line-delimited JSON. One object per line. Requests may omit `"jsonrpc":"2.0"`.
Responses and notifications always include it.

Spawn:

```bash
mow rpc [--session ID] [--continue] [--allow-write] [--allow-shell] …
```

Child stdin = requests. Child stdout = responses + notifications.
Stderr is Engine logs — do not parse it.

## Handshake

1. `{"id":1,"method":"version"}` → `rpc` must be `"3"`.
2. `{"id":2,"method":"session"}` → workspace, model, session_id.
3. `{"id":3,"method":"status"}` → busy, allow_write, allow_shell, ask_mode.
4. If the UI wants ask mode: `perm.set` `{mode:"ask"}` (default server is
   fail-open auto).
5. `transcript` to seed history on `--session` / `--continue`.
6. `slash.list` for `/help`.

## Methods

| Method | Control? | Params | Result |
|---|---|---|---|
| `prompt` | no (worker) | `{text}` | `{text, session_id, run_id, stop_reason, usage}` |
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
| `version` | yes | — | `{name, version, rpc, package}` |
| `ping` | yes | — | `"pong"` |

Control methods are answered concurrently with an in-flight `prompt`.
**`transcript` can return before the current turn is appended.** After
`prompt` completes, call `transcript` again if you need the stored turns.

`prompt` / `slash` share a worker queue (depth 4). Overflow → error, retry.

Caps: prompt/steer text 512k runes; event deltas 8k; transcript content 32k
per message; stdin line 1 MiB.

### `status`

`busy`, `run_id`, `session_id`, `workspace`, `model`, `wire`,
`allow_write`, `allow_shell`, `ask_mode`, `pending_perm`.

### `slash`

`name` is the token without a required leading slash (`review` or `/review`).
`args:["help"]` (also `-h`, `--help`, `?`) returns usage without `Run`.

`exclusive && status.busy` → JSON-RPC error (not an envelope): refuse like
Go mowi.

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
| token / `loop.token` | append live answer (`delta`) |
| reasoning / thinking | activity / collapsible thought |
| `tool.start` / `tool.end` | activity band + tool line (`duration_ms` on end) |
| `run.start` / `run.end` | busy; usage on end |
| `harness.delegate.chunk` | peer live buffer (`agent`, `delta`) — **not** host answer |
| `harness.delegate.progress` | peer phase (`thought`, `tool`, `prompt`) |
| `harness.delegate.usage` | add to peer token chip |
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

## What is not on the wire (yet)

- Model picker catalog — v1 can omit
- Theme / keys — UI-local config
- Binary diffs — events carry text; UI pretty-prints
- Spawning peers — Engine only
