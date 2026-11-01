# Architecture decision records

The decisions behind Sandstorm v0.6 as it is built: a gVisor sandbox fleet,
one master and many nodes, that oversells by moving idle sandboxes down
through four tiers. Each record says what was decided, why, and what follows
from it. [ADR 1](0001-record-architecture-decisions.md) says how these are
written and kept.

| # | Decision | Status |
|---|---|---|
| [1](0001-record-architecture-decisions.md) | Record architecture decisions | Accepted |
| [2](0002-gvisor-runtime-no-kvm.md) | Run sandboxes on gVisor, so no host needs KVM | Accepted |
| [3](0003-four-tiers.md) | Four tiers: hot, frozen, snapshot, detached | Accepted |
| [4](0004-master-and-node.md) | A master holds desired state in SQLite; nodes run sandboxes | Accepted |
| [5](0005-control-plane-mtls.md) | The control plane runs on mutual TLS, in either direction | Accepted |
| [6](0006-pools-are-migration-domains.md) | A pool is a migration domain, set by a computed fingerprint | Accepted |
| [7](0007-wake-on-traffic.md) | The node's gateway wakes a sandbox when a request reaches it | Accepted |
| [8](0008-nodes-protect-themselves.md) | Each node decides for itself whether it can take work | Accepted |
| [9](0009-wake-makes-room.md) | A wake makes room on its node, spares sandboxes in use, and has 2 s to start | Accepted |
| [10](0010-one-binary.md) | One binary: master, node and panel | Accepted |
| [11](0011-firecracker-backend.md) | Firecracker as an optional second backend, behind one trait | Accepted, and run on KVM and under PVM |

## Earlier designs

- **v0.6 RFC** (Chinese), written before most of v0.6 was built. Several of
  its proposals were changed or dropped, so read it as history: last version
  at
  [`7b39861`](https://github.com/deeplethe/sandstorm/blob/7b3986150b71feeece318ce1b4475f3c291d3aae/DESIGN-v0.6.md).
- **v0.2 to v0.5, the Firecracker runtime**: `DESIGN.md`, `DESIGN-v0.4*.md`,
  `DESIGN-v0.5-diff-snapshot-chains.md` and `docs/design/` are in the
  [`v0.5.3` tag](https://github.com/deeplethe/sandstorm/tree/v0.5.3).

## Adding a record

Copy the shape of an existing one: Status, Context, Decision, Consequences.
Take the next number, name the file `NNNN-short-title.md`, and add a row
above. Record a decision once it is made and in the code. To change one,
write a new record and mark the old one `Superseded by NNNN`.
