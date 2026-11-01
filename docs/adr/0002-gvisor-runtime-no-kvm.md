# 2. Run sandboxes on gVisor, so no host needs KVM

Date: 2026-10-24

## Status

Accepted. Amended by [ADR 11](0011-firecracker-backend.md): Firecracker is now
an optional second backend, and gVisor stays the default.

## Context

Up to v0.5.3 every sandbox was a Firecracker microVM. Firecracker needs
`/dev/kvm`, and most cloud instances do not expose nested virtualisation.
None of the three machines Sandstorm is developed and measured on (bm, bj,
hk) has `/dev/kvm`; their `/proc/cpuinfo` does not even list the `vmx` or
`svm` flags. Operators are in the same position, so a runtime that needs KVM
does not run on the machines they actually have.

gVisor (`runsc`) runs in user space and needs no KVM. It also has what the
tiers in [ADR 3](0003-four-tiers.md) are built from: a sandbox is a cgroup
that can be frozen, and `runsc checkpoint` / `runsc restore` take it to disk
and back.

## Decision

`sandstormd` runs every sandbox as a gVisor `runsc` container. gVisor is the
only backend in the tree.

A Firecracker backend for hosts that do have KVM is still the plan, but it
has to be built into the node runtime in `crates/sandstorm-core`. The v0.5
Firecracker code (`sandstorm-cli`, `sandstorm-vmm`, `sandstorm-uffd`,
`experiments/`) never fed `sandstormd` and has left the tree; it is in the
[`v0.5.3` tag](https://github.com/deeplethe/sandstorm/tree/v0.5.3).

## Consequences

- Sandstorm runs on ordinary cloud instances with no special host setup for
  virtualisation.
- The isolation boundary is weaker than a microVM's. Every sandbox shares the
  host kernel, so an escape needs a gVisor Sentry bug plus a gap in the
  seccomp filter behind it, not a KVM or Firecracker bug.
  [`docs/SECURITY.md`](../SECURITY.md) states the boundary per backend rather
  than claiming a microVM boundary.
- Every v0.6 measurement (tier costs, wake latency, fleet density) comes from
  `runsc`. gVisor charges guest memory as file/shmem and Firecracker as anon,
  so the numbers do not carry over to a future Firecracker backend; its tiers
  have to be measured again on a host with `/dev/kvm`.
- Features that relied on Firecracker are gone: live fork, hugepages and
  diff snapshots. The API answers those options with 410.
- A checkpoint records the `runsc` release and the host CPU features, which
  limits which nodes can restore it ([ADR 6](0006-pools-are-migration-domains.md)).
