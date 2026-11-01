# 11. Firecracker as an optional second backend, behind one trait

Date: 2026-10-29

## Status

Accepted, and run on KVM and under PVM: see
[the bare-metal comparison](../../bench/baremetal/results/2026-10-30/README.md)
and [the latency round](../../bench/fleet/results/2026-10-31/README.md). Amends
[ADR 2](0002-gvisor-runtime-no-kvm.md).

## Context

[ADR 2](0002-gvisor-runtime-no-kvm.md) made gVisor the backend because most
machines have no KVM. Where KVM does exist, a microVM is the stronger
isolation boundary, and the v0.5 runtime had density tricks gVisor cannot
match: forks of one image sharing page cache, and diff snapshots.

v0.5 got those from a patched Firecracker run with `--no-seccomp`, and never
fed `sandstormd`. A second backend has to sit inside the node runtime and
keep the four tiers ([ADR 3](0003-four-tiers.md)), the netns per sandbox and
the cgroup per sandbox exactly as they are.

## Decision

The node runtime talks to a `Backend` trait (`node/backend.rs`). `runsc` is
the default; `sandstormd --backend firecracker` selects
`node/firecracker.rs`, on **upstream** Firecracker with no fork.

- **The backend owns preparation.** A VM wants a block device, not an overlay
  directory, so the trait covers `template_root`, `prepare`, `layer`,
  `unmount` and `teardown` as well as boot, restore, checkpoint, kill and exec.
  `bundles/<id>/config.json` still marks a sandbox as on this node.
- **Same cgroup, same tiers.** Firecracker writes itself into
  `/sys/fs/cgroup/sandstorm/<id>` before it starts, so frozen
  (`cgroup.freeze`), memory accounting and `cgroup.kill` are one code path for
  both backends. It runs under `setsid` in the sandbox's netns and outlives
  `sandstormd`. The backend keeps no state in memory; every path follows from
  the id, and a kill checks the socket path in `/proc/<pid>/cmdline` first.
- **Memory and disk travel together.** A guest's page cache refers to disk
  blocks, so pairing memory from one moment with a disk from another corrupts
  the filesystem. Every image (tag, snapshot tier, warm template) holds
  `vmstate`, `mem.0 … mem.k` and a `rootfs.ext4` reflinked while the VM is
  paused. The drive path is relative and Firecracker runs in the sandbox's own
  `layers/<id>/disk/`, so a restore opens its own copy and never the parent's.
  Restore still re-points the drive before resuming: `snapshot/load` with
  `resume_vm: false`, `PATCH /drives/rootfs`, `PATCH /vm Resumed`, and a kill
  on any failure (the order #321 worked out).
- **Reflink is required.** The work directory must support `FICLONE` (XFS
  with `reflink=1`, or btrfs). The backend refuses to start otherwise; it
  never falls back to full copies.
- **Shared page cache.** Every fork of a tag maps one memory file
  (`MAP_PRIVATE`, File backend).
- **Diff chains.** Dirty-page tracking is on from boot and every load. A
  checkpoint whose memory came from a chain shorter than 8 writes a Diff
  snapshot; otherwise a Full one. Chains are hard-linked into each image, so
  every image stands alone and dropping a tag breaks no other. The first
  restore assembles the chain (reflink `mem.0`, then copy each diff's data
  ranges by `SEEK_DATA`/`SEEK_HOLE`) into a cached `.mem.assembled`.
- **One guest address.** Every guest is `10.42.0.2` behind `tap0`
  (`10.42.0.1/30`) in its netns, with DNAT in and MASQUERADE out, so a
  snapshot's network config is right in any netns. The master and gateway
  still see `10.86.x.y`.
- **The guest** runs `rootfs-init/sandstorm-init.sh` as PID 1 and the agent on
  port 8888, which templates therefore cannot use. The template's args, env
  and cwd arrive hex-encoded as `sandstorm.app=` on the kernel command line
  (2048-byte limit). `exec` goes through the agent; the clock is set through
  it after every restore.

Not carried over: live fork (memfd + `UFFD_WP` needs a patched Firecracker
and `--no-seccomp`; upstream #6270 and #5951 are unmerged), hugepages
(snapshot restore needs the UFFD backend, which rules out the shared page
cache), and volumes (refused for now; a VM needs a block device, not a bind
mount).

## Consequences

- One node runs one backend. A pool's fingerprint ([ADR 6](0006-pools-are-migration-domains.md))
  includes the engine version, so images never cross backends.
- A host for `--backend firecracker` needs `/dev/kvm`, the Firecracker binary,
  an uncompressed guest vmlinux (`scripts/install-guest-kernel.sh`), a
  reflink-capable work directory, and templates as `templates/<name>/rootfs.ext4`.
- A tag sent to another node carries its whole `rootfs.ext4` and chain; sparse
  ranges and reflinks are not kept in transit. `prefetch_snapshot` reads the
  disk image as well as memory.
- To verify first on KVM, in this order:
  1. That a Diff snapshot after a load holds only pages dirtied since the
     load. If not, chains miss pages and guest memory is corrupt.
  2. The full lifecycle: create, freeze, checkpoint, restore, fork from a tag,
     detach, destroy.
  3. Whether the guest clock needs setting after a long freeze too
     (`cgroup.freeze` stops vCPU threads, unlike Firecracker's own pause).
  4. The tier numbers. gVisor's figures do not transfer: the two account
     memory differently (file/shmem against anon).
- Boot sets `track_dirty_pages`, but `snapshot/load` still sends
  `enable_diff_snapshots`, its older name, which recent Firecracker accepts as
  deprecated. Check it on the version you install.
