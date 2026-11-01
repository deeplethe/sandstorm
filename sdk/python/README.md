# sandstorm — Python SDK

Python client for a [Sandstorm](https://github.com/deeplethe/sandstorm) master.
Its `Sandbox` class supports a small E2B-style command API, but uses
Sandstorm's native HTTP API. To use the official `e2b` SDK, see the
[E2B recipe](../../recipes/e2b-codeinterpreter/README.md).

The SDK talks to the master over HTTP, so all it needs is the master's address
and credentials. It has no dependencies, and nothing else has to be installed
on the client machine.

```bash
pip install sandstorm-sdk
export SANDSTORM_URL=https://master.example:8080
export SANDSTORM_TOKEN="$(sudo cat /var/lib/sandstorm/pki/api-token)"
export SANDSTORM_CA=ca.pem
```

A client verifies the name it connects to, and a master's certificate names only
`127.0.0.1`, `localhost` and its `--bind` address. So start this one with
`--tls-san master.example`.

```python
from sandstorm import Sandbox

with Sandbox("python") as sandbox:              # a template on the master
    result = sandbox.commands.run("python3 -c 'print(2 + 2)'")
    print(result.stdout)        # 4
    print(result.exit_code)     # 0
```

Or with explicit lifecycle:

```python
sandbox = Sandbox.create("python")
result = sandbox.commands.run("echo hello")
sandbox.kill()
```

`Sandbox` returns once a node reports the sandbox running, and removes it if
it never starts. A sandbox that has not started within `start_timeout` seconds
(default 60) raises `RuntimeError`, with the node's reason when there is one.
A template whose program fails to load raises after about 15 seconds. A program
that starts and then exits is not caught here: `Sandbox` returns, and commands
fail while the node restarts it.

## Connecting to the master

| Variable | Meaning |
|---|---|
| `SANDSTORM_URL` | Master base URL. Defaults to `https://127.0.0.1:8080`, where a master listens unless you bound it elsewhere. |
| `SANDSTORM_TOKEN` | The master's API token, in `pki/api-token` under its state directory. |
| `SANDSTORM_CA` | PEM bundle to verify an `https://` master. A master signs its certificate with a CA of its own, so this is usually its `pki/ca.pem`. |
| `SANDSTORM_TEMPLATE` | Template for `Sandbox()` when none is given. |
| `SANDSTORM_TAG` | Snapshot tag for `Sandbox()` when no template or tag is given. |

To reach a master in code rather than through the environment, pass a
`Controller`:

```python
from sandstorm import Controller, Sandbox

master = Controller(base_url="https://master.example:8080", token="...", ca_file="ca.pem")
sandbox = Sandbox("python", controller=master)
```

## Starting from a snapshot

Pass `tag` to fork from a snapshot instead of starting a template cold:

```python
with Sandbox(tag="checkpoint-1") as sandbox:
    print(sandbox.commands.run("ls /work").stdout)
```

## Many sandboxes

`Sandbox` holds one sandbox. To fork many at once, use `Controller` and wrap
each child with `Sandbox.connect`. Each child has its own token, returned only
when it is created:

```python
master = Controller()
children = master.spawn_sandboxes("checkpoint-1", n=5)
sandboxes = [
    Sandbox.connect(child["id"], child["token"], controller=master)
    for child in children
]
```

`kill` and a `with` block terminate a connected sandbox too.

`Controller` is also how to branch a running sandbox into a new tag and manage
snapshots.

## Running a snippet

`eval` runs code through the template's `eval_argv`, `python3 -c` by default,
and returns what it printed. It is a fresh process each time, and an exception
raises `RuntimeError` with the snippet's stderr:

```python
with Sandbox("python") as sandbox:
    sandbox.eval("print(2 + 2)")    # '4\n'
```

## Errors

A call the master refuses raises `ControllerError`, with the HTTP status in
`status` and the response in `body`. For example, a command in a sandbox that
has gone idle is a 409 until it is brought back with `POST
/v1/sandboxes/:id/touch`.

A command that exits non-zero is not an error: read `exit_code` on the result.

Outbound network access follows the master's egress policy. It is off by
default; configure it in the console or through `POST /v1/egress`.

## Status

Pre-alpha. Currently supports:
- `Sandbox(template)` / `Sandbox(tag=...)` / `Sandbox.create(...)` — create one sandbox
- `Sandbox.connect(id, token)` — wrap a sandbox that already exists
- `sandbox.commands.run(cmd)` — run command, get stdout/stderr/exit_code
- `sandbox.eval(code)` — run a snippet, get what it printed
- `sandbox.ping()` — ask the master about the sandbox
- `sandbox.kill()` — terminate

Not yet:
- `sandbox.files.read/write` — filesystem operations
- Streaming output
