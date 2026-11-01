# Latency on three hosts, 2026-10-31

The README's Latency table, measured under gVisor and under Firecracker on KVM and PVM, all on `dev` at `d58ab87`, the same release binary (`78165dce`). It has #400, #401 and #387, none of which the [2026-10-30](../2026-10-30/README.md) round had. Idle timers: frozen after 15 s, on disk after 60 s.

| | Backend | Machines |
|---|---|---|
| `gvisor/` | gVisor | six nodes, 2 cores, 2 GB, no KVM; master f1; rebooted first |
| `kvm/` | Firecracker v1.17.0 | one machine, 2 × Xeon E5-2637 v2, 16 threads, 31 GB, master and node in one process |
| `pvm/` | Firecracker v1.17.0 | bm, EPYC 9K65, 2 vCPU, 3.8 GB, CubeSandbox's PVM host kernel and guest kernel, master and node in one process |

The hosts differ, so read each column on its own. bm also held two other masters, idle, about 1.5 GB between them, which leaves it 2.4 GB and room for about 8 Firecracker guests of the 64 MiB app. So its counts are smaller: 8 in the idle sweep where the others ran 14, 20 under traffic where they ran 40, 3 per size where they ran 5, herds up to 8, and no 1 GiB app. 20 against about 8 awake is the same overcommit as gVisor's 40 against about 12.

All templates are one ext4 image under Firecracker, with the guest memory the spec sets, and the same apps as gVisor.

## Wake

p50, 200 only.

| | gVisor | KVM | PVM |
|---|---:|---:|---:|
| Idle 1-10 s, not yet frozen, 64 MiB | 6 ms | 6 ms | 2 ms |
| Frozen, idle 18-50 s, 64 MiB | 103 ms | 165 ms (35-50 s) | 78 ms (35 s) |
| On disk, idle 65-170 s, 64 MiB | 172 ms | 234 ms | see below |
| Restore, 16 / 64 / 256 MiB | 131 / 148 / 720 ms | 150 / 150 / 499 ms | 82 / 135 / 454 ms |
| Restore, 512 MiB / 1 GiB | 1,051 / 3,335 ms | 696 / 1,368 ms | 414 ms / not run |

Samples: restores 60 per size under gVisor, 10 on KVM, 6 on PVM; 512 MiB and 1 GiB 6 under gVisor, **1** under Firecracker. On KVM the sandboxes idle 18 and 25 s had not been frozen yet, so frozen is 35-50 s; on PVM likewise 35 s, 4 samples. One of KVM's 84 idle probes failed.

The idle sweep is `sleep`: six nodes, two runs under gVisor; one node, six runs on KVM and four on PVM.

On PVM some wakes took about 2 s, for three reasons, read from the node's log: at 65 s the node was short of memory and put another sandbox to disk first, a checkpoint of about 1.9 s; at 100 and 170 s that evicted sandbox came back and its restore spent 2.04 s refused by its own node, a bug fixed in #414; and a wake behind a checkpoint the idle timers had started waited up to 2 s on this host's 2 vCPUs. The restore row is the figure with none of these. KVM, with room to spare, evicted nothing.

## Under traffic

40 sandboxes per node (20 on PVM), 64 MiB app, Zipf 1.1, open loop, 150 s at each rate, three runs (two on PVM). Refused, p50, p99.

| Per node | gVisor | KVM | PVM |
|---|---:|---:|---:|
| 1 req/s | 2.1%, 9.8 ms, 2.6 s | 0.2%, 50 ms, 1.9 s | 0%, 13.5 ms, 3.4 s |
| 2 req/s | 0.2%, 6.8 ms, 1.5 s | 0%, 8.5 ms, 794 ms | 0%, 10.2 ms, 2.5 s |
| 4 req/s | 0.1%, 6.1 ms, 1.7 s | 0%, 7.4 ms, 830 ms | 0%, 8.2 ms, 3.6 s |
| 8 req/s | 1.6%, 7.3 ms, 2.4 s | 0%, 6.5 ms, 877 ms | 0%, 3.7 ms, 1.8 s |

40,456, 6,737 and 4,483 requests.

## Herds

Of K woken at the same instant on one node, how many answered, mean of the runs.

| K | gVisor | KVM | PVM |
|---:|---:|---:|---:|
| 1 | 1 | 1 | 1 |
| 4 | 4 | 4 | 4 |
| 8 | - | - | 5.5 |
| 10 | 6 | 10 | - |
| 20 | 6 | 19.5 | - |
| 40 | 6 | 28 | - |

## Kept in memory, per node

| | gVisor | KVM | PVM |
|---|---:|---:|---:|
| web | 76 | 293 | 45 |
| 64 MiB app | 18 | 160 | 34 |
| 256 MiB app | 6 | 85 | 18 |

Of 900 asked for, what was up after settling, per node: two runs and 60 s under gVisor, two runs and 60 s (256 MiB) or 120 s on KVM, three runs and 120 s on PVM. Not comparable with the 2026-10-30 figures, which allowed 10 s: since #387 a node counts what is still starting, and 10 s now shows 29 web sandboxes where 60 s shows 76. KVM stopped at its swap floor. PVM's runs were close together: 45-46, 30-38 and 17-20. These count what stayed up with swap, not what answers quickly: swap reached 1.5-2.0 GB (`pvm/walls/mem.log`), and only about 8 of the 64 MiB app fit without it. The other columns count the same way.

## What went wrong on the way

- `workloads.py` waited for the disk by asking `runsc list`, so under Firecracker it returned at once and `sizes`, `heavy` and `herd` measured hot sandboxes: 5 ms restores. It now reads the sandbox's cgroup, and those three were rerun. `sizes`, `heavy` and `herd` here are the reruns.
- The `mixed80` runs were left out, or stopped: the README does not use them. Not in this directory.
- The first PVM round used the other columns' counts, 14 and 40, on a host with room for about 8. Everything then waited on swap: hot requests took 0.9 s, 48% were refused at 8 req/s, and creates took up to 79 s. Three runs on the same host found the cause: 14 sandboxes with reclaim on or off stayed slow; 8 answered in 3 ms, swapping in 4 pages a second at most against 1,568. That round is not here.
- KVM's first walls were refused or left pending behind the last teardown. Rerun from an idle node.

`#405` changed `rootfs-init` after this build; these images have the older init.
