# Security policy

sandstorm is alpha software. The threat model and current guarantees are
documented below so operators can decide what workload they are
willing to point at it. It describes the v0.6 development tree, which is not
released yet.

## Which boundary you actually get

Each node runs gVisor (`--backend runsc`, the default) or Firecracker
(`--backend firecracker`), and the boundary depends on which.

| | gVisor backend | Firecracker backend |
|---|---|---|
| Sandbox | gVisor `runsc` container | Firecracker microVM |
| Guest kernel | the gVisor Sentry, in user space | its own Linux kernel |
| Host kernel | **shared with every sandbox** | reached only through KVM |
| Escaping needs | a Sentry bug, or a hole in the host seccomp behind it | a KVM or Firecracker bug |
| Needs `/dev/kvm` | no | yes |

**Why gVisor is the default.** It can run without `/dev/kvm`. The
Firecracker backend requires a host with KVM and a guest kernel image.

So, concretely:

1. **The host kernel and the runtime (`runsc` or Firecracker) are part of
   the TCB.** A compromised host can do anything to its sandboxes. sandstorm
   does not try to protect against a hostile administrator.

2. **Sandboxes are mutually untrusted, with the boundary above.** Each
   sandbox gets its own network namespace, cgroup and writable layer. Under
   gVisor all sandboxes on a node share the host kernel, so a Sentry escape
   can reach the host and its other sandboxes: do not treat gVisor as
   equivalent to a VM for hostile code. Firecracker gives each sandbox its
   own guest kernel, and is the stronger choice on a host with KVM.

3. **The control plane is a root credential.** The API's bearer token grants
   full control over the sandboxes and snapshots of every node attached to
   that master. Nodes additionally authenticate with client certificates
   (see below), but the API token is not scoped -- treat it as root.

## Default posture

These are `sandstormd`'s defaults.

| Concern | Default | Notes |
|---|---|---|
| Daemon bind | `127.0.0.1:8080` (loopback only) | `--bind` to expose it; do not do that without the token below |
| Control-plane TLS | **on** | `--insecure-transport` disables both TLS and node client-certificate checks |
| Node authentication | **client certificates, required with TLS** | A node's identity is the certificate the master issued it, not the node id in its messages. `POST /v1/node/revoke/:id` stops one |
| Registration | single-use token, 15 min, carrying the CA fingerprint to pin | So a stand-in master fails the handshake before the token is sent to it |
| API authentication | **on**, token generated on first run | `--api-token-file`; `--api-open` disables it and warns. Any caller able to reach an open API can mint node tokens and control sandboxes |
| Per-sandbox netns | **always** | Not an option any more. `per_child_netns` is accepted and ignored |
| Sibling and egress traffic | **refused** or dropped (`FORWARD`), unless a sandbox's own policy lets it out | Covers siblings and the internet without relying on `ip_forward` |
| Host services on the veth | **dropped** (`INPUT`) | sshd listens on `0.0.0.0`, so it answers on the veth too; only established replies are let back |
| Cold sandbox ports | Frozen: direct connections are **refused** (`SANDSTORM-COLD`, TCP reset). Snapshotted: no namespace, so unreachable | The HTTP gateway wakes a cold sandbox and retries before forwarding the request |
| Per-sandbox memory cap | gVisor: none; Firecracker: 512 MiB guest memory by default | `memory_limit_mib` configures the limit; Firecracker adds 64 MiB for the VMM to its cgroup `memory.max` |
| API audit log | **none** | The middleware exists but is not attached to the API router |

Sandbox gateways listen on `0.0.0.0` at their node ports and check
`x-sandstorm-sandbox-token`, as do the API's `exec`, `eval` and `ping`
endpoints. Tokens have 128 random bits, as do generated IDs (`sb-` plus 32 hex
characters). A caller-supplied ID need not be random, and no ID replaces a
token.

## Images

A template from an image is built by root on a node, from layers nothing has
vouched for. So a layer is not unpacked by `tar`, which protects an archive from
itself but not from the layer before it: a link left by one layer would let the
next write anywhere on the host. Each path is resolved the way a process chrooted
into the rootfs would resolve it, devices are skipped, and each blob must hash to
its digest. The image's program then runs in a sandbox like any other.

## Known gaps

Stated because each is the kind of thing that reads as covered when it is
not:

- **Node gateways serve plain HTTP.** Control-plane TLS does not encrypt
  their sandbox tokens or application traffic.
- **The E2B front door does not authenticate requests to sandbox ports.** It
  adds the sandbox token itself, so anyone who can reach it and knows an E2B
  sandbox's ID reaches that sandbox's ports, stopped only by the application's
  own authentication. The E2B API still requires a key.
- **Egress is per address, not per name.** There is no domain filtering and no
  inspection of what leaves. A sandbox with `internet` can reach any public
  address, including the public addresses of the master and of other nodes, so
  those rely on their own authentication. It is off by default; see
  [Egress](API.md#egress).
- **One API token, no scopes or tenants.** The `owner` column exists in the
  schema and nothing reads it.

## Kubernetes

There is no supported manifest yet. A node needs `privileged: true`,
`runAsUser: 0` and a writable `/sys/fs/cgroup` hostPath mount, plus `/dev/kvm`
for Firecracker, so a compromised `sandstormd` pod can compromise its host. If
you run one anyway:

- Treat the pod's bearer token like SSH-root on the node. Rotate it on any
  access change.
- Pin the pod to a dedicated node pool. Do not co-schedule untrusted tenants.
- For multi-tenant deployments, run one master per tenant on dedicated nodes.
  The API token is not scoped, so sharing a master means sharing root over every
  tenant's sandboxes.

## Concurrency caps

The current daemon's limits are:

- **Fork width.** One request may ask for at most `MAX_FORK_WIDTH` (256)
  children of a tag.
- **Admission.** A node stops accepting new sandboxes below a free-memory
  floor or above a memory-pressure average (`--admit-min-free-mib`,
  `--admit-max-psi`). This exists because without it the master kept
  accepting creates until the machine stopped answering SSH; the measured
  curve is in `master/api.rs`.
- **Checkpoint size** is not bounded. Measured at 50.2 MiB per sandbox, so a
  thousand snapshotted sandboxes is about 50 GB of disk. Admission looks at
  memory and pressure, not disk.

## TLS

There are no certificate flags any more, and nothing to obtain: the master
runs its own CA under `--pki-dir`, creating it on first start, and issues
itself a server certificate and each node a client certificate from it.
rustls 0.23 with the `ring` provider; TLS 1.2 and 1.3, no legacy suites.

Operationally:

- **The control plane's PKI is internal and should stay that way.** It
  authenticates nodes to the master and the master to its nodes; it is not
  a web PKI and a public CA has no role in it. Keep `--pki-dir` backed up
  with the state database: losing the CA key means re-registering every
  node.
- **A node pins the CA on first contact** through the fingerprint carried in
  its registration token (`<fingerprint>.<secret>`). Pass the token the
  master printed, unaltered; splitting it loses the pin and the node will
  warn that it cannot verify the master.
- **Revoke, do not rotate, to remove a node:** `POST /v1/node/revoke/:id`.
  Its parked connection is dropped at once.
- **TLS does not imply API authentication.** The bearer token is separate
  and is on by default; `--api-open` turns it off and says so loudly.
- **A TLS-terminating proxy with a different CA breaks the pin.** The master
  rejects node requests without a recognized client certificate and does not
  fall back to a claimed node ID. Only `--insecure-transport` turns that off.

## What sandstorm does not do (yet)

- **Control-plane HA and failover.** Multi-node scheduling is implemented;
  control-plane HA and failover are not.
- **Quotas beyond memory.** `cpu.max`, `io.max` and `pids.max` are not
  wired through the API.
- **Third-party security audit.** Not started. Will be required
  before sandstorm claims a "production" status badge.

## Reporting a vulnerability

Email `security@deeplethe.com`. Please do not open a public issue for
security reports. We aim to acknowledge within 72 hours and ship a fix
or mitigation within 14 days for confirmed issues.

## Supported versions

Pre-1.0 releases receive fixes only on the latest minor. Fixed
vulnerabilities are recorded in the CHANGELOG under the release that
fixed them, with the affected versions.
