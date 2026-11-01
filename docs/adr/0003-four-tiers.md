# 3. Four tiers: hot, frozen, snapshot, detached

Date: 2026-10-23

## Status

Accepted

## Context

The point of Sandstorm is to oversell: most sandboxes sit idle most of the
time, so a sandbox that is not working should cost neither CPU nor memory.
v0.5 had two states, running and "snapshot to disk, then kill", with nothing
in between.

The RFC proposed four tiers, one of them `cold`: a frozen sandbox whose
memory is reclaimed by lowering `memory.high`, so it stays in place but out
of RAM. Measured on bm on 2026-10-23, this worked for one sandbox (90% of its
memory released in 7 ms) and failed under concurrency: with 8 sandboxes,
batched, serial and rate-limited reclaim all met their target 0 of 8 times
and ran into the 85 s timeout, while checkpointing the same 8 took 1.1 s and
released 413 MiB. Several cgroups doing direct reclaim at once contend for
the same LRU and swap path, which tuning does not get around. `memory.reclaim`
left the pages in RAM as swap cache.

## Decision

A sandbox is in one of four tiers (`crates/sandstorm-core/src/tier.rs`):

| Tier | What it is | Gives back |
|---|---|---|
| hot | running | -- |
| frozen | `cgroup.freeze=1` on the same node | CPU |
| snapshot | `runsc checkpoint` to the node's local disk, process gone | CPU and memory |
| detached | nothing on any node; the writable layer archived to the volume | CPU, memory and local disk |

The `cold` tier is dropped.

- Demotion goes one tier at a time and is decided by idle time; the master's
  demotion loop (`master/demote.rs`) only ever moves desired state one tier
  colder.
- Promotion always goes straight to hot and happens when a request arrives
  ([ADR 7](0007-wake-on-traffic.md)).
- Deleting a sandbox works from any tier.
- A detached sandbox comes back by a cold start plus unpacking its layer from
  the volume; its memory state is not kept. Moving to another machine is done
  this way, never by moving a checkpoint.

## Consequences

- Frozen does not free memory: it shares hot's memory budget. Memory density
  comes from the snapshot tier.
- Snapshot does not free local disk: the writable layer stays on the node.
  Disk density comes from the detached tier.
- The memory a frozen sandbox holds is still squeezed where it can be: its
  pages, and the cold pages of hot sandboxes, go to compressed swap (zswap or
  zram) when the node has it (`--reclaim-frozen`, `--reclaim-hot`). Hot
  reclaim (`node/reclaim.rs`) is paced by each sandbox's own memory stalls;
  neither is the concurrent `memory.high` squeeze that was rejected.
- Open TCP connections inside a sandbox can time out while it is frozen or on
  disk.
- A sandbox's memory state does not survive a move to another node; only its
  volume does.
