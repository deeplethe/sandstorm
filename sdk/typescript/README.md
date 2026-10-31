# @deeplethe/sandstorm

TypeScript client for [Sandstorm](https://github.com/deeplethe/sandstorm): gVisor and Firecracker sandboxes across a fleet of nodes.

```bash
npm install @deeplethe/sandstorm
# or pnpm add @deeplethe/sandstorm
```

Requires Node 20+. (The code only needs the global `fetch`, which Node 18
had too; the floor is 20 because that is what the SDK is tested on -- Node
18 has been end-of-life since April 2025.)

## Connecting to a master

A master listens on `https://127.0.0.1:8080` unless it was bound elsewhere, and
signs its certificate with a CA of its own. So the public trust store cannot
verify it, and until you give the SDK that CA every call fails with
`fetch failed`. Get the CA from the master's `pki/ca.pem`, or fetch it:

```bash
curl -k https://127.0.0.1:8080/ca.pem -o ca.pem
export SANDSTORM_CA=$PWD/ca.pem
export SANDSTORM_TOKEN=$(sudo cat /var/lib/sandstorm/pki/api-token)
```

`-k` is for this one download. Check `ca.pem` against the fingerprint in a
registration token before you trust it. There is deliberately no way to turn
verification off in the SDK: an unverified control plane carries the API token
and every command run inside a sandbox.

The certificate names `127.0.0.1`, `localhost` and the master's `--bind`
address. To connect by any other name, start the master with
`--tls-san master.example`.

## Quick start

```ts
import { Controller, Sandbox } from "@deeplethe/sandstorm";

// Either: create + use + cleanup. A master starts sandboxes from registered
// templates; `ctrl.listTemplates()` names them.
const result = await Sandbox.with(
  { template: "python" },
  async (sb) => sb.exec(["python3", "-c", "print(2+2)"]),
);
console.log(result.stdout); // "4\n"

// Or: long-lived sandbox with BRANCH
const source = await Sandbox.create({ template: "python" });
await source.exec(["python3", "/opt/agent.py"]);

// BRANCH: checkpoint it into a tag. The parent keeps running, after a short
// pause, and children inherit its exact state.
const checkpoint = await source.branch({ tag: "after-warmup" });

// Fan out N children from the checkpoint.
const ctrl = new Controller();
const children = await ctrl.spawnSandboxes({
  snapshotTag: checkpoint.tag,
  n: 4,
});
```

`ctrl.createSandbox` and `ctrl.spawnSandboxes` answer before a node has started
the sandbox. `Sandbox.create` waits for it, and for anything you made yourself,
`await new Sandbox(ctrl, info).waitUntilRunning()` does the same.

## API surface

Surface parity with the Python SDK (`pip install sandstorm-sdk`):

| Python | TypeScript |
|---|---|
| `Controller.list_snapshots()` | `ctrl.listSnapshots()` |
| `Controller.delete_snapshot(tag)` | `ctrl.deleteSnapshot(tag)` |
| `Controller.create_sandbox(...)` | `ctrl.createSandbox({ template, snapshotTag, ... })` |
| `Controller.spawn_sandboxes(...)` | `ctrl.spawnSandboxes({ snapshotTag, n, ... })` |
| `Controller.list_sandboxes()` | `ctrl.listSandboxes()` |
| `Controller.get_sandbox(id)` | `ctrl.getSandbox(id)` |
| `Controller.kill_sandbox(id)` | `ctrl.killSandbox(id)` |
| `Controller.branch_sandbox(id, tag)` | `ctrl.branchSandbox(id, { tag })` |
| `Controller.exec_command(id, args)` | `ctrl.execCommand(id, args, { timeoutSecs })` |
| `Controller.eval_code(id, code)` | `ctrl.evalCode(id, code, token, timeoutSecs)` |
| `Controller.ping_sandbox(id)` | `ctrl.pingSandbox(id)` |

Also here: `ctrl.listTemplates()`, `ctrl.getSnapshot(tag)`,
`ctrl.touchSandbox(id)`, `ctrl.setSandboxTier(id, tier)` and
`ctrl.retrySandbox(id)`.

Snake-case API field names are preserved over the wire (the master
expects them), but TypeScript-side argument names are camelCase.

### `Sandbox` (higher-level wrapper)

```ts
const sb = await Sandbox.create({ template: "python" });
const result = await sb.exec(["echo", "hi"]);
const printed = await sb.eval("print(2+2)"); // "4\n"
const branch = await sb.branch({ tag: "checkpoint-1" });
await sb.kill();
```

`eval` returns what the snippet printed, so use `print(...)`, not a bare
expression. `branch` needs a hot sandbox: the master answers 409 otherwise, and
`sb.touch()` wakes it.

Keep the creation response, including its `token`: v0.6 requires this sandbox
token in addition to the API Bearer credential for exec/eval/ping. `Sandbox.create`
forwards it automatically, as does `new Sandbox(ctrl, info)` when `info` is the
creation result. GET/LIST responses do not contain it. Missing or incorrect
tokens produce the server's authentication error.

`Sandbox.with(options, fn)` is the recommended pattern for short-lived
work — automatic cleanup even on exception.

## Configuration

```ts
new Controller({
  baseUrl: "https://127.0.0.1:8080",      // or env SANDSTORM_URL
  token: "abc123",                         // or env SANDSTORM_TOKEN
  caFile: "ca.pem",                        // or env SANDSTORM_CA
  timeoutMs: 60_000,                       // default
  fetch: customFetch,                      // optional (testing / older Node)
});
```

`caFile` replaces the trust store rather than adding to it: the master's CA is
the only issuer that can sign for it. It needs Node, and the file is read on the
first request, so a wrong path fails there. A custom `fetch` does its own TLS, so
passing it with `caFile` throws, and `SANDSTORM_CA` does not apply to it.

`timeoutMs` covers ordinary calls. Exec and eval wait for `timeoutSecs` plus 25 s
when that is longer, and a branch for 185 s, so the master's own answer arrives
first.

## Error handling

```ts
import { ControllerError } from "@deeplethe/sandstorm";

try {
  await ctrl.getSandbox("sb-missing");
} catch (e) {
  if (e instanceof ControllerError && e.status === 404) {
    // sandbox doesn't exist
  }
}
```

## Testing

```bash
pnpm install
pnpm test
```

Mock fetch by passing your own implementation to the `Controller` constructor (see [`tests/controller.test.ts`](./tests/controller.test.ts)).

## See also

- [sandstorm-mcp](https://pypi.org/project/sandstorm-mcp/) — MCP server for Claude Desktop / Cursor / Cline
- [sandstorm-action](https://github.com/deeplethe/sandstorm-action) — GitHub Action
- [Python SDK](https://pypi.org/project/sandstorm-sdk/) — `pip install sandstorm-sdk`

## License

Apache-2.0.
