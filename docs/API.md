# Sandstorm HTTP API (v1)

The master serves this API over TLS on `127.0.0.1:8080` by default (`--bind`).
Clients verify it against the deployment's `ca.pem`, which is
[`GET /ca.pem`](#get-capem) when you do not have the file. Its certificate names
`127.0.0.1`, `localhost` and the `--bind` address. Add any other name clients
or a node's `--master-url` use with `--tls-san`, which can be repeated, or they
will refuse it. Every
route needs the API token unless this page says otherwise; the token is in
`/var/lib/sandstorm/pki/api-token`.

```http
Authorization: Bearer <api-token>
```

Every 4xx and 5xx response carries `{ "error": "human-readable message" }`.

The official E2B SDKs use a separate listener, with `--e2b-bind` and
`--e2b-domain`. See [`recipes/e2b-codeinterpreter`](../recipes/e2b-codeinterpreter/README.md).

## Quick look

| Route | What it does |
|---|---|
| `GET /healthz` | Liveness probe, no token |
| `GET /ca.pem` | The deployment's CA certificate, no token |
| `GET /metrics` | Prometheus metrics |
| `GET /v1/nodes`, `POST /v1/nodes` | List nodes, take over a listening one |
| `POST /v1/node-tokens`, `POST /v1/node-bootstrap` | Let a node join by dialling the master |
| `POST /v1/node/revoke/:id` | Stop trusting a node |
| `POST /v1/nodes/:id/backend` | Move a node to another backend |
| `GET /v1/templates`, `POST /v1/templates` | List and register templates |
| `POST /v1/templates/from-image`, `GET /v1/templates/pulls` | Make a template from a container image, and see how it goes |
| `GET /v1/sandboxes`, `POST /v1/sandboxes` | List sandboxes, start or fork them |
| `GET /v1/sandboxes/:id`, `DELETE /v1/sandboxes/:id` | Read or delete one |
| `POST /v1/sandboxes/:id/{touch,tier,retry}` | Wake it, move it, retry a failure |
| `POST /v1/sandboxes/:id/egress` | Change what it may reach |
| `GET /v1/egress`, `POST /v1/egress` | What new sandboxes may reach by default |
| `POST /v1/sandboxes/:id/{exec,eval,ping}` | Run something in it, with its sandbox token (`ping` also takes `GET`) |
| `POST /v1/sandboxes/:id/branch` | Checkpoint it into a tag |
| `GET /v1/snapshots`, `GET /v1/snapshots/:tag/info`, `DELETE /v1/snapshots/:tag` | Manage tags |

The console's own routes, `/v1/console/{now,series,events}`, are for the console
and may change. So are `/v1/node/{poll,traffic,result}`, which nodes use.

## Status

### GET /healthz

Answers `ok` with a 200. It needs no token, so a load balancer can probe it.

### GET /ca.pem

The deployment's CA certificate, as PEM. It needs no token, because a client that
does not have it yet cannot verify the connection it would send one over. So the
first fetch is unverified:

```bash
curl -k https://master:8080/ca.pem -o ca.pem
```

Check it before you trust it: `ca_fingerprint` from `POST /v1/node-bootstrap`, and
the start of a registration token, are its SHA-256.

### GET /metrics

Prometheus text. It needs the token, unlike `/healthz`: it names every node and
its free memory, which is a map of the deployment.

```yaml
scrape_configs:
  - job_name: sandstorm
    scheme: https
    authorization: { credentials_file: /etc/sandstorm/api-token }
    tls_config: { ca_file: /var/lib/sandstorm/pki/ca.pem }
    static_configs: [{ targets: ["master:8080"] }]
```

| Series | Tells you |
|---|---|
| `sandstorm_sandbox_creates_total{outcome}`, `sandstorm_sandbox_deletes_total`, `sandstorm_demotions_total{to}` | What the control plane was asked to do |
| `sandstorm_http_request_duration_seconds{method,route}`, `sandstorm_store_lock_wait_seconds`, `sandstorm_store_lock_held_seconds`, `sandstorm_demote_sweep_seconds` | How long it took |
| `sandstorm_nodes{state}`, `sandstorm_node_*{node,pool}`, `sandstorm_sandboxes{tier}`, `sandstorm_sandboxes_failed`, `sandstorm_snapshots`, `sandstorm_snapshot_bytes` | What the fleet looks like now |

- `route` is the matched path, so `/v1/sandboxes/:id` is one series.
- `/v1/node/poll` is a long poll. Its duration is the hold (`--hold-secs`), not a
  cost, so do not average it in with the rest.
- A capacity refusal is a 503, so it counts under
  `sandstorm_http_responses_total{class="server_error"}`.
  `sandstorm_sandbox_creates_total{outcome="refused"}` separates "the fleet is
  full" from "the master is broken".

## Nodes

A node joins one of two ways. Both run the same reconciliation; only which side
opens the connection differs.

| | The node runs | How it joins | Use it when |
|---|---|---|---|
| **Listen** | `serve --node --listen 0.0.0.0:80` | The master is given its address and pairing code | The master can reach the node |
| **Pull** | `serve --node --master-url …` | The node redeems a registration token | Only the node can reach the master, such as behind NAT |

### GET /v1/nodes

A list of `{id, pool, fingerprint, max_sandboxes, connected, last_seen_at,
free_mib, psi_some_avg10}`, plus `not_admitting` with the reason when a node
would refuse a new sandbox.

### POST /v1/nodes

Take over a node that is already installed and listening.

```json
{
  "address": "10.2.4.17:80",
  "pairing_code": "68fed7b4…<64 hex>.<32 hex>",
  "node_id": "f3",
  "pool": "default"
}
```

- `address` may omit the port, which defaults to 80. Write IPv6 as `[addr]:port`.
- `node_id` defaults to what the node reports: its `--node-id`, else its hostname.
- `pool` is a request. A node whose host fingerprint differs from the pool's joins
  `<pool>#<fp8>` instead, as a registering node does.

The pairing code is `<certificate SHA-256>.<secret>`, which the node writes to
`/var/lib/sandstorm/pki/node/pairing-code` (root only) on first start. The master
pins the node's certificate by the first half and presents the second on every
request. The code does not change, so it is a standing credential for that node.
To change it, delete `pairing-secret` beside it and restart the node.

| Status | Meaning |
|---|---|
| `201` | Added. Answers `{id, pool, address, fingerprint, max_sandboxes}` |
| `400` | Malformed address or code; nothing was dialled |
| `409` | A node with this id is connected and polling; stop that service first |
| `502` | The node could not be reached, presented a different certificate, or refused the secret; the body says which |

### POST /v1/node-tokens

Make a single-use registration token for a pull node. Both fields are optional.

```json
{ "pool": "default", "ttl_secs": 900 }
```

Answers `{token, pool, expires_at}`. The token starts with the CA fingerprint, so
the node can pin the master before it sends the secret.

### POST /v1/node-bootstrap

The same, as a command to run on the new machine.

```json
{ "master_url": "https://10.2.4.1:8080", "pool": "default", "backend": "auto" }
```

`master_url` is required, because the master cannot tell how a new machine reaches
it. `backend` is `auto` (Firecracker where the machine has `/dev/kvm`, gVisor
elsewhere), `gvisor` or `firecracker`; without it the installer picks gVisor.
The answer adds `install_command` and `ca_fingerprint` to the token's fields.

### POST /v1/node/revoke/:id

Stop trusting a node. A polling node's certificate is refused from then on, and a
listening node is no longer contacted. Adding a listening node again with its
pairing code lifts the revocation.

### POST /v1/nodes/:id/backend

Move a node to another backend.

```json
{ "backend": "firecracker" }
```

`backend` is `gvisor` (or `runsc`) or `firecracker`. The node checks it can run
that backend, rewrites its `--backend-file` and restarts, and its fingerprint then
puts it in another host group. Nothing is placed on it meanwhile.

Answers `{node, from, to, restarting, message}`; `restarting` is false if it was
on that backend already. Answers `409` while the node holds any sandbox or
snapshot, since those are in the old backend's format; when it is not answering;
when it was installed without a backend file; or when it refuses, with its reason.

## Templates

### GET /v1/templates

A list of template names.

### POST /v1/templates

Register a template. Its files must already be on the nodes. To have them put
there, use [`from-image`](#post-v1templatesfrom-image).

```json
{
  "name": "base",
  "rootfs": "rootfs",
  "spec": { "args": ["/usr/bin/python3", "/app/srv.py"], "memory_max_mib": 512 }
}
```

`spec` is kept as given; `args`, `env`, `cwd`, `ro_binds` and `memory_max_mib` are
the fields a node reads. Answers `201`.

### POST /v1/templates/from-image

Have every node pull an image from its registry and keep it as a template. Nodes
need no Docker, and a node that joins later pulls it too.

```json
{ "name": "python", "image": "python:3.12-slim", "memory_max_mib": 256 }
```

| Field | Meaning |
|---|---|
| `name` | The template's name |
| `image` | As for `docker pull`. Docker Hub unless it names a registry |
| `memory_max_mib` | Memory ceiling. 512 by default |
| `args` | What to run, in place of the image's `Entrypoint` and `Cmd` |
| `env` | Added to the image's `Env` |
| `port` | In place of the lowest port the image exposes |

Answers `202` at once. A sandbox can start as soon as one node has it, on the
nodes that do, and until then `POST /v1/sandboxes` answers `409`. Sent again with
the same image it tries again, on the nodes that failed and any that have not
finished. A name that is already something else is refused with `409`.

- Public images only: a registry that wants a login is reported as failed.
- Plain HTTP only to `localhost` and `127.0.0.1`.
- 32 GiB at most, compressed.
- A program runs as root, whatever the image's `User` says.
- On Firecracker nodes the image needs `bash` and `python3`, which the guest's
  init and agent run on. One without them fails on those nodes and says so.

### GET /v1/templates/pulls

```json
[{
  "name": "python",
  "image": "python:3.12-slim",
  "ready": true,
  "nodes": [
    { "node_id": "f3", "state": "ready" },
    { "node_id": "f5", "state": "failed", "error": "the image has no linux/arm64 variant" }
  ]
}]
```

`state` is `pulling`, `ready` or `failed`.

## Sandboxes

### POST /v1/sandboxes

Start one from a template, or fork from a tag. Exactly one of `template` and
`snapshot_tag` is required.

```json
{ "template": "base", "port": 8000 }
```

| Field | Meaning |
|---|---|
| `template` | Start from this template |
| `snapshot_tag` | Fork from this tag |
| `n` | How many to make, at most 256. Also selects a list as the answer |
| `id` | Your own id. It need not be random, and it does not replace a token |
| `pool`, `node_id` | Where to place it |
| `backend` | Only on nodes running `gvisor` or `firecracker`. For a template |
| `port` | The port the gateway forwards to by default |
| `volume` | A named volume to attach |
| `memory_limit_mib` | Memory ceiling, overriding the template's |
| `egress` | What it may reach, in place of the [default](#egress) |
| `per_child_netns` | Accepted and ignored: every sandbox has its own namespace |

`live_fork` or `hugepages` set to `true` is refused with `410 Gone`.

Answers `201` with a [Sandbox](#sandbox), or with a list of them when `n` is
given. Each carries its `token` this one time: no other response has it, so keep
it. A `503` means no node can take it now.

**Where forks run.** A tag's image is written on the node that branched it. If
that node is one the master connects to, each child goes to the least loaded node
whose host fingerprint matches the tag's, and a node without the image fetches it
from the source node's agent port the first time. So every node of the pool must
be able to reach the others on that port. If the source node dials in to the
master, it has no address to fetch from, and every child runs on it. `node_id`
names a node outright, and is refused with `409` if its fingerprint differs from
the tag's or the image cannot be fetched to it.

### GET /v1/sandboxes

Every sandbox, or a page of them with these query parameters. The total that
match is in `x-total-count`.

| Parameter | Meaning |
|---|---|
| `tier` | `hot`, `frozen`, `snapshot` or `detached` |
| `node` | On this node |
| `q` | Part of the id or the template name |
| `failing` | Only those carrying an error |
| `limit`, `offset` | The page |

### GET /v1/sandboxes/:id

One [Sandbox](#sandbox), without its token.

### DELETE /v1/sandboxes/:id

Delete it, on whichever tier it is. Answers `204`.

### POST /v1/sandboxes/:id/touch

Mark it used, which brings it back to hot. Answers a [Sandbox](#sandbox).

### POST /v1/sandboxes/:id/tier

Ask for a tier now, rather than waiting for the idle policy. The master walks the
tiers in between. Answers a [Sandbox](#sandbox).

```json
{ "tier": "snapshot" }
```

### POST /v1/sandboxes/:id/retry

Clear a sandbox's failure so its node tries again. Answers a [Sandbox](#sandbox).

### Reaching a sandbox

`exec`, `eval` and `ping` need the sandbox's token as well as the API token, and
so does its gateway at `http://<node>:<gateway_port>/`, with `X-Sandstorm-Port`
naming a port other than its default.

```http
x-sandstorm-sandbox-token: <token>
```

### POST /v1/sandboxes/:id/exec

Run a command and wait for it. `timeout_secs` defaults to 30.

```json
{ "args": ["python3", "-c", "print(2+2)"], "timeout_secs": 30 }
```

```json
{ "stdout": "4\n", "stderr": "", "exit_code": 0 }
```

### POST /v1/sandboxes/:id/eval

Run a snippet in the sandbox with the template's `eval_argv`, `python3 -c` by
default. `timeout_secs` defaults to 30. The answer has `result`, what the snippet
printed, or `error`, never both.

```json
{ "code": "print(2+2)" }
```

```json
{ "result": "4\n", "exit_code": 0 }
```

### POST /v1/sandboxes/:id/ping

Check the sandbox answers.

```json
{ "ok": true, "tier": "hot", "guest_addr": "10.86.0.2:8000", "latency_ms": 3 }
```

### POST /v1/sandboxes/:id/branch

Checkpoint a running sandbox into a new tag to fork from. The parent keeps
running, after a short pause. The call returns once the node has written the
image, so a fork from the tag cannot race ahead of it.

```json
{ "tag": "checkpoint-1" }
```

`tag` is required, and is a plain name: no slashes, no leading dots. The fields
`mode`, `live`, `wait`, `diff` and `measure_diff` were removed in v0.6. Only
`"mode": "full"` is still accepted, since that is what a branch is.

| Status | Meaning |
|---|---|
| `201` | Written. Answers a [Snapshot](#snapshot) |
| `400` | `tag` is not a plain name |
| `404` | No such sandbox |
| `409` | The tag exists, or the sandbox is not hot; use `touch` first |
| `410` | A removed field was set |
| `500` | The checkpoint failed on the node |
| `504` | The node did not finish within 180 s |

## Egress

A sandbox reaches nothing beyond its own node unless it is allowed to, and by
default none is. Policy, on a sandbox or as the default:

```json
{ "internet": true, "allow": ["10.1.0.0/16"], "deny": ["203.0.113.0/24"] }
```

| Field | Meaning |
|---|---|
| `internet` | Public addresses. Never private ranges, other sandboxes or the cloud metadata service |
| `allow` | IPv4 addresses and CIDRs always reachable, private ones included |
| `deny` | IPv4 addresses and CIDRs not reachable through `internet`. `allow` wins over it |

At most 64 entries between the lists. A domain is refused with `400`: a rule
holds addresses. A sandbox that may reach names needs a resolver it can reach,
which `internet` gives it; with `allow` alone, list the resolver too.

### POST /v1/sandboxes/:id/egress

Change it, in any tier. What the body names is replaced and the rest stays, so
`{ "internet": true }` keeps the lists. Answers the policy as stored. The node
applies it with its next command, which is after any `exec` it is running, and a
connection already open then dies if the new policy forbids it.

### GET /v1/egress

The policy a new sandbox gets when it does not give one.

### POST /v1/egress

Change it, by the same rule. Only sandboxes created from then on are affected.

Through the [E2B API](../recipes/e2b-codeinterpreter/README.md), `allow_internet_access: false`
closes a sandbox and `network.allowOut` and `network.denyOut` add to these lists.
At creation `true` does not open one: older SDKs send it unless told otherwise,
so the default decides. `PUT /sandboxes/:id/network` changes it afterwards, where
it does.

## Snapshots

A tag is made with `branch`.

### GET /v1/snapshots

A list of [Snapshot](#snapshot).

### GET /v1/snapshots/:tag/info

One [Snapshot](#snapshot), plus `children`: how many sandboxes are still forked
from it.

### DELETE /v1/snapshots/:tag

Answers `204`, `404` if there is no such tag, or `409` while sandboxes are still
forked from it. Each node holding a copy of the image drops it the next time it
reports holding it.

## Objects

### Sandbox

```json
{
  "id": "sb-0123456789abcdef0123456789abcdef",
  "node_id": "f3",
  "pool": "default",
  "template": "app64",
  "tier": "hot",
  "port": 8000,
  "gateway_port": 20011,
  "ip": "10.86.0.2",
  "guest_addr": "10.86.0.2:8000",
  "netns": "sandstorm-sb-0123456789abcdef0123456789abcdef",
  "egress": { "internet": false },
  "volume": null,
  "created_at": 1793348196,
  "created_at_unix": 1793348196,
  "last_active_at": 1793348200
}
```

- `gateway_port` is absent when none was free. The sandbox is then reachable only
  from its node.
- `ip` and `guest_addr` are absent until the node reports them.
- `snapshot_tag` is the tag it was forked from, and `memory_limit_mib` the override
  in force, when there is one.
- `last_error` and `error_attempts` say why it is not up, when it is not.

### Snapshot

```json
{
  "tag": "checkpoint-1",
  "node_id": "f3",
  "template": "app64",
  "from_sandbox": "sb-0123456789abcdef0123456789abcdef",
  "branched_from": "sb-0123456789abcdef0123456789abcdef",
  "dir": "tags/checkpoint-1",
  "size_bytes": 67108864,
  "created_at": 1793348300,
  "created_at_unix": 1793348300
}
```
