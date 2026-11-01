# Sandstorm against other open-source sandboxes, on bare metal — 2026-10-30

One machine, every runtime on it in turn, the same two programs, the same harness.

- **Machine:** 2 × Intel Xeon E5-2637 v2 (Ivy Bridge, 2013; 8 cores, 16 threads), 31 GB, a SATA SSD, Ubuntu 24.04, kernel 6.8, native KVM.
- **Programs:** `web`, a Python HTTP server, and `app64`, which holds a 64 MiB numpy heap and reads all of it on every request (`bench/fleet/apps`).
- **Start:** one instance at a time, from the call that starts it to its first 200, polled every 5 ms; 20 after one warm-up; p50.
- **Memory:** 100 instances kept running, each answering before the next starts; 30 s to settle; MemAvailable before and after, divided by the count; then 20 of them called at random. Nothing is tiered or paused in this table: Sandstorm ran with `--idle-frozen-secs 86400 --reclaim-hot off --reclaim-frozen off`, so its sandboxes stayed hot like everyone else's.
- **Harness:** [`compare.py`](../../compare.py), [`setup.sh`](../../setup.sh), [`fork.py`](../../fork.py), [`fork_msb.py`](../../fork_msb.py), [`fork_cube.py`](../../fork_cube.py). Raw rows: `run1.jsonl` (the runtimes that are not Sandstorm, plus microsandbox), `run2.jsonl` (Sandstorm), `run3.jsonl` (Kata), `run4.jsonl` (CubeSandbox), `run5.jsonl` (Sandstorm Firecracker, warm) and `run6.jsonl` (Sandstorm gVisor), both with all of deeplethe/sandstorm#380, `fork.jsonl` (the last row of each Sandstorm backend is the one below).

## Start and memory

| | isolation | start, web / app64 | memory per instance, web / app64 |
|---|---|---:|---:|
| plain process | none | 67 / 200 ms | 7.8 / 79.7 MiB |
| bubblewrap | namespaces, host network | 74 / 195 ms | 8.3 / 66.9 MiB |
| Docker (runc) | namespaces | 256 / 414 ms | 15.2 / 88.5 MiB |
| Podman (crun) | namespaces | 296 / 429 ms | 10.8 / 84.0 MiB |
| Docker + gVisor | gVisor | 327 / 660 ms | 30.8 / 106.8 MiB |
| **Sandstorm, gVisor** | gVisor | **234 / 545 ms**⁴ | **24.1 / 97.9 MiB** |
| **Sandstorm, gVisor, warm template** | gVisor | **107 / 123 ms**⁴ | 23.3 / 97.3 MiB |
| Kata 4.2 (runtime-rs, QEMU) | VM | 3,005 / 3,397 ms | 223 / 311 MiB¹ |
| microsandbox 0.7.8 (libkrun) | VM | 1,117 / 1,119 ms | 80.2 / 166.2 MiB |
| CubeSandbox 0.7.3 | VM | 72 / 78 ms | **8.9** / 12.2 MiB |
| **Sandstorm, Firecracker** | VM | 636 / 964 ms² | 80.0 / 166.1 MiB |
| **Sandstorm, Firecracker, warm template** | VM | **62 / 74 ms**³ | 9.9 / **10.5 MiB**³ |

Every runtime answered 20 of 20 when sampled.

¹ Kata keeps every guest's memory in shared memory for virtio-fs, so `/dev/shm` (16 GB here) is its ceiling: 81 `web` guests, 57 `app64`. Past it a new guest fails to start, and at 57 `app64` none of the 20 sampled answered; its figure is from 40.
² After the fixes in deeplethe/sandstorm#380, each measured on its own: 1,953 / 2,120 ms before them. `run2.jsonl` has 868 / 1,174 ms, measured before the last two (`quiet`, the agent starting the program first).
³ `run5.jsonl`, with all of deeplethe/sandstorm#380. `run2.jsonl` has 129 / 199 ms and 10.8 / 11.9 MiB from before it.
⁴ `run6.jsonl`, with all of deeplethe/sandstorm#380. `run2.jsonl` has 308 / 624 ms cold and 168 / 186 ms warm from before it.

Every CubeSandbox and warm Sandstorm sandbox starts from a snapshot of the program already running, and maps that memory privately, so a page only costs a sandbox once it writes it. `app64` only reads its heap after starting, which is why it costs those two barely more than `web`; a program that rewrote its heap would cost more.

## Snapshot and fork

The source is a running `app64` started from a template; polled every 5 ms.

| p50 | Sandstorm, Firecracker | Sandstorm, gVisor | CubeSandbox | microsandbox³ |
|---|---:|---:|---:|---:|
| snapshot a running sandbox | **27 ms** | 342 ms | 252 ms | — |
| start 1 from the snapshot, to its first answer | **110 ms** | 117 ms | 133 ms | — |
| clone 1: snapshot, then start | **~137 ms** | ~459 ms | 542 ms | 658 ms |
| clone 10, until all answer | **~528 ms** | ~1,340 ms | 973 ms | 835 ms |
| clone 20, until all answer | **~980 ms** | ~2,280 ms | 1,046 ms | 1,152 ms |

Sandstorm's clone is `branch` then one fork call with `n`, so the sum is approximate. Its snapshot was 14-34 ms over six; one run straight after the 100-instance memory run, with the host still writing back, had 41-52. CubeSandbox's is its SDK's `clone(n, concurrency=n)`: a snapshot, then n creates at once (0.7.3 has no `/fork`).
³ `msb fork` copies a running sandbox in one step. Its children get no port forward and cannot be called from the host, so its time is until `msb fork` returns with them running, not until they answer; each was checked from inside afterwards.

## What it says

- **Against CubeSandbox,** the fastest of the rest: a warm Sandstorm Firecracker sandbox starts faster (62 against 72 ms for `web`, 74 against 78 for `app64`) in about the same memory (9.9 / 10.5 against 8.9 / 12.2 MiB). It snapshots a running sandbox 9× faster, clones one 4× faster, and clones 10 and 20 faster too.
- **Against the other VM runtimes,** it starts 15-18× faster than microsandbox and 46-48× faster than Kata, in an eighth to a thirtieth of the memory. Cold, it is still faster than both.
- **Against Docker with gVisor,** the same isolation, Sandstorm on gVisor starts 1.2-1.4× faster cold (234 / 545 against 327 / 660 ms), 3-5× faster from a warm template (107 / 123 ms), and holds 8-24% less per instance.
- A fork of n children is created one after another on the node, about 50 ms each; CubeSandbox and microsandbox create them in parallel, which is where they would gain on a larger n.

## Found and fixed on the way

All in deeplethe/sandstorm#380:

- The gateway retried a sandbox that had not started listening yet with a backoff up to 500 ms, holding a first answer 300-370 ms past the program listening. Sandstorm gVisor cold `app64`: 941 → 616 ms.
- Firecracker cold start: i8042 probing, the agent importing numpy before starting the program, a SYN resent only after 1 s, and above all ARP for a guest still booting asked again only after 1 s; then `quiet` and the program started first. `web` 1,953 → 636 ms.
- A fixed tap MAC, without which pinning the guest's neighbour entry made every warm start 43 s.
- A guest hostname that resolves without DNS, which cost 20 s per Firecracker sandbox on bm.
- Two master-and-node processes on one machine sharing gateway ports: the second now refuses to start.
- KVM dirty logging on every restored VM, kept so its next snapshot could be a diff, which made each page the guest touched first fault in on its own: warm `app64` 103 → 67 ms. Writes are now tracked with the kernel's soft-dirty bits, as CubeSandbox does, and a snapshot copies just those pages: 52 → 27 ms.
- The netns pool's refill waited for a quiet spell that back-to-back starts never leave, until the pool was empty: from the 33rd start on, 185-236 ms. It now refills in time, and reuses a destroyed sandbox's namespace (its conntrack table flushed) rather than building one under load.

## Not covered

- E2B self-hosted (Nomad and Consul) was not set up.
- No load: idle sandboxes that each answered single requests.
- One machine, one run of each; the p90s are in the raw rows.
- bubblewrap shares the host's network here, so it did the least work of all.

## Rerun on 2026-10-31, on `dev` at c40260a

The figure at the top of the README comes from this rerun, not the tables above.

- **KVM machine:** `run7.jsonl` (Sandstorm Firecracker, cold and warm), `run8.jsonl` (everything else but Kata), `run9.jsonl` (CubeSandbox), `fork.jsonl` (the last row of each runtime).
- **Under PVM:** `pvm/bm.jsonl`, on bm (EPYC 9K65, 2 vCPU, 3.6 GB) with CubeSandbox's PVM host kernel.
- **CubeSandbox cold:** the `cubesandbox-cold` rows of `run9.jsonl` and `pvm/bm.jsonl`. `cubemastercli multirun` with a template's stored create request, its `appsnapshot` annotations removed, so the VM boots; `period_ms` 5 and `failure_threshold` 6000, because the default 60 tries at 5 ms give up after 300 ms. p50 of 20 after one, no failures. Its API has no such path. Cubelet's own timing of `template redo` read 2.7 s on KVM and 1.8 s under PVM; that overstates it, and is not used.

Warm `app64` on the KVM machine moved between the two days: Sandstorm 74 → 100 ms and CubeSandbox 78 → 129 ms, while the other runtimes barely moved. The same Sandstorm binary gave both figures, and neither rebuilding the image nor compacting memory brought the old one back. Not explained.

Under PVM the two are level: Sandstorm 52 / 83 ms, CubeSandbox 53 / 87 ms. Warm has to run on its own there: straight after the cold runs, on two cores, it read 63 / 98 ms (the earlier rows of `pvm/bm.jsonl`).
