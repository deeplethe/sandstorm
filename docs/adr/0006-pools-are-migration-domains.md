# 6. A pool is a migration domain, set by a computed fingerprint

Date: 2026-10-28

## Status

Accepted

## Context

A `runsc` checkpoint only restores on a compatible host. On 2026-10-23 a
snapshot from bm (AMD EPYC, kernel 7.0) failed on bj (Intel Xeon, kernel
5.15) twice: once because the `runsc` releases differed by a week, once with
`incompatible FeatureSet` listing missing CPU features. Firecracker has the
same constraint, so this is a property of checkpoint/restore rather than of
gVisor.

Until this decision a node computed a host fingerprint at registration and
nothing used it, so a mismatched node could join a pool and the problem
appeared only when a restore on it failed.

## Decision

- **A pool is a set of nodes whose snapshots can restore on each other.**
- **The fingerprint is computed, never configured**
  (`node::runtime::fingerprint`): a hash of the `runsc` version, the host
  kernel release, and the CPU vendor and flags.
- A node whose fingerprint does not match the pool it asked for joins a
  derived pool, `<pool>#<fp>`, and the master logs why. An upgraded node
  leaves its old pool on its own instead of quietly poisoning it.
- Each tag records the fingerprint it was baked under
  (`snapshots.fingerprint`). A fork directed at a node that cannot restore it
  is refused with 409 at placement, not on the node.
- What moves between the nodes of a pool is a tag's checkpoint image, so a
  fork can land on any node that can restore it; the image is fetched from
  the node that holds it. A sandbox that leaves its node comes back by a cold
  start from its volume ([ADR 3](0003-four-tiers.md)).

## Consequences

- An operator cannot configure a pool in which restores are bound to fail.
- Upgrading `runsc` or the kernel on a node changes its fingerprint and moves
  it out of its pool, and forks of tags baked under the old fingerprint are
  no longer placed on it.
- A matching fingerprint is necessary; whether it is sufficient rests on
  measurement. A bj-to-hk restore once reported success and then died with
  SIGSEGV, but in the 2026-10-26 round, checkpoints moved between three nodes
  with identical fingerprints restored every time, heap intact, including one
  image seeding children on all three at once
  ([`bench/fleet/results/2026-10-26`](../../bench/fleet/results/2026-10-26/README.md)).
