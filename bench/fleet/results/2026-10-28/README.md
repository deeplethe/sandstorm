# Fleet run, 2026-10-28

The same six nodes and master as [2026-10-26](../2026-10-26/README.md), with
the wake fixes in [#360](https://github.com/deeplethe/sandstorm/pull/360) and
[#361](https://github.com/deeplethe/sandstorm/pull/361), run again with every
measurement repeated, and the same apps run on the same machines without Sandstorm.

## Setup

| | |
|---|---|
| Master | f1: 1 × Intel Xeon Gold 6148, 2 vCPU, 1.9 GB |
| Nodes | f3, f5, f7, f8, f9, f10: AMD EPYC 7K62, 2 vCPU, 1.9 GB, kernel 7.0.0-14, a 2 GB swap file with zswap in front |
| Runtime | runsc `release-20261019.0` |
| Admission | defaults: 512 MiB free floor, memory PSI 8.0 |
| Apps | [`apps.sh`](../../apps.sh): `web` (a Python HTTP server and nothing else) and `app16` … `app1024` (Python with numpy and a heap that every request reads) |
| Idle timers | frozen after 15 s and snapshot after 60 s; 5 s and 15 s for the fills |
| Requests | from each node to its own gateway, over loopback: no network in any latency |

Builds:

- **v6** ran the node suite ([`suite-node.sh`](../../suite-node.sh), every node at once): a wake makes room, reports what it put away, waits out memory pressure; held-back creates are never quarantined.
- **v7** ran the walls, the fills and two more 80-per-node runs. It adds only the clearing away of sandboxes that died (the last bug below); wake and admission are v6's.
- **v12** is `dev` at 872682b, with #360 and #361 merged, and ran the traffic runs again on the same parameters (`v12/`). #361 spares a sandbox only while a request is in flight, puts away the one least in demand, and refuses a wake that cannot start within 2 s instead of queueing it.

## Results

`summary.py` prints every table below from the files here (`python3 bench/fleet/summary.py v12/*/mixed40-*.jsonl.gz`, ...).

### Under traffic

40 app64 per node, Zipf 1.1 popularity, open-loop arrivals at 1, 2, 4 and 8 req/s per node for 150 s each, three runs on six nodes. The same parameters as round 3, which ran on five nodes.

| Per node | Round 3: refused | v6: refused | v6: p99 | v12: refused | v12: p50 | v12: p99 |
|---|---:|---:|---:|---:|---:|---:|
| 1 req/s | 13% | 1.2% | 2.4 s | **1.1%** | 11.2 ms | 2.4 s |
| 2 req/s | 26% | 0.0% | 2.0 s | **0.1%** | 7.2 ms | 2.0 s |
| 4 req/s | 32% | 0.1% | 2.4 s | **0.2%** | 6.6 ms | 1.9 s |
| 8 req/s | 33% | 2.6% | 12.1 s | **2.5%** | 8.0 ms | 2.3 s |

40,506 requests at v12. p50 and p99 are of the requests answered. Run by run, 8 req/s refused 3.0%, 2.5% and 2.0%, and 1 req/s 1.7%, 0.9% and 0.9%.

What remains at 8 req/s is a node under its floor with nothing left that it may put away. At v6 the tail was the price of not refusing: a request waited for a restore, and at 8 req/s for other restores ahead of it, up to 12 s. At v12 a wake that cannot start within 2 s is refused instead, and refusals did not rise for it: 2.6% then, 2.5% now, while the p99 at 8 req/s fell from 12.1 s to 2.3 s.

With 80 per node (two runs): 1.1%, 0.1%, 0.3% and 10.6% refused at v12, p99 2.9 s, 1.9 s, 1.9 s and 3.2 s. v6 refused 12.5% at 8 req/s with a p99 of 58 s. At 8 req/s that is past what a node with room for about 12 awake can turn over.

The builds in between, on the same parameters:

| Per node | v3: makes room, one run | v3, three runs | v5 | v4, wake PSI ceiling lifted to 1000 |
|---|---:|---:|---:|---:|
| 1 req/s | 2.1% | 1.8% | 1.9% | 1.1% |
| 2 req/s | 1.3% | 0.5% | 0.0% | 0.1% |
| 4 req/s | 1.6% | 0.7% | 1.5% | 0.1% |
| 8 req/s | 10.7% | 8.3% | 12.4% | 2.4%, p99 14 s |

At v3 the refusals left at 8 req/s were almost all the memory-pressure ceiling, tripped for a few seconds by the node's own restores; lifting it showed what they cost, and v6 waits up to 8 s for pressure to ease instead of refusing at once.

### 20,000 at 20/s, then called at random

Each fill creates 20,000 `web` sandboxes through the master at 20/s ([`workloads.py fill`](../../workloads.py)), waits for the nodes to settle, then calls 100 picked at random on every node ([`workloads.py probe`](../../workloads.py)), every one on disk by then.

| | Up with nothing retried by hand | Quarantined | Answered when called | p50 / p99 |
|---|---:|---:|---:|---:|
| before the held-back fix, fill 1 | 18,758 | 1,246 | 582 of 600 (all 18 that failed were never created) | 1.24 s / 3.1 s |
| before, fill 2 | 18,439 | 1,565 | — | — |
| v5, fill 3 | 20,000 | 0 | 600 of 600, once pressure had eased; 527 of 600 straight after | 1.25 s / 3.5 s |
| v5, fill 4 | 20,000 | 0 | 592 of 600 | 1.27 s / 2.9 s |
| **v7, fill 5** | **20,000** | **0** | **598 of 600** | 1.30 s / 5.5 s |
| **v7, fill 6** | **20,000** | **0** | **600 of 600** | 1.29 s / 2.8 s |

The two that failed in fill 5 waited out the pressure for 8 s, but it stayed at 12–15 for those seconds, and they were refused as designed. `v7/fills/` has each fill's create log, the watcher's samples and the probes.

### Kept in memory, wake and restore

| | |
|---|---|
| Every sandbox kept in memory, per node (v7, two runs) | web 47.7, app16 23.4, app64 12.6, app256 5.7 |
| Wake by idle time, 64 MiB app (v6) | hot 3.8 ms; frozen 103 ms, of which 11 ms is the thaw; from disk 230 ms |
| Restore p50 by heap (v6) | 16 MiB 178 ms, 64 MiB 198 ms, 256 MiB 974 ms, 512 MiB 1.1 s, 1 GiB 3.6 s |
| Create to first answer, p50 (v6) | app16 1.7 s, app64 1.4 s, app256 1.7 s: five at once per node, status and app each polled every 0.25 s. One at a time, see below |
| Herds: N woken at once per node (v6) | 20: 240 of 240, slowest 6.1 s; 40: 478 of 478, slowest 12.7 s. Round 3 woke about 11 of either and refused the rest |

### Kept in memory by heap, sandbox against plain process

`heaps/`, taken on 2026-10-29 on the same six nodes, for the figure's "Kept in memory" panel. Its first version took 512 MiB and 1 GiB from the heavy runs, which start a fixed 2 and 1 per node, and had no plain-process count for either, so those two sizes showed one bar each. Now every size is measured both ways, on the same day, back to back:

- **gVisor sandbox:** the hot wall (`suite-master.sh ... walls`, `SETTLE=90`), two runs per size, mean up per node. A create a node holds back is retried as memory frees and counts as failed until then; at the default 10 s settle the 16 MiB wall read 13.8 with 890 MiB still free (`walls-run.log` is the 90 s run). At 90 s every refusal left is the node's 512 MiB floor.
- **Plain process:** `baselines.py --runtimes process`, twice on every node, median of 12.

| Per node | 16 MiB | 64 MiB | 256 MiB | 512 MiB | 1 GiB |
|---|---:|---:|---:|---:|---:|
| plain process | 23.5 | 10 | 3 | 2 | 1 |
| gVisor sandbox | 18.75 | 10.67 | 4.08 | 2.17 | 1.08 |

Both are lower than the v7 walls and the baselines below: the nodes were not clean. Each still held 40–150 MiB of zswap pool and about 300 MiB of swap from earlier runs, and had about 1.2 GB available idle against 1.3 GB or more on 2026-10-28. The two rows are comparable with each other, not with those.

### The same apps without Sandstorm

[`baselines.py`](../../baselines.py) on every node, twice, with Sandstorm stopped and the machine clean: each runtime runs the app with the host's `/usr` read-only, as a template does, and starts instances one at a time, each answering before the next, until 512 MiB is left. Median per node over 12 runs:

| | web | app16 | app64 | app256 | web: start to first answer |
|---|---:|---:|---:|---:|---:|
| plain process | 96 | 27 | 11 | 4 | 52 ms |
| bubblewrap | 94 | 27 | 11 | 4 | 52 ms |
| Podman (crun) | 88 | 27 | 12 | 4 | 238 ms |
| Docker (runc) | 60 | 23 | 10 | 3 | 264 ms |
| Docker + gVisor | 32 | 17 | 9 | 3 | 349 ms |
| Sandstorm, all kept in memory | 48 | 23 | 13 | 6 | |

### Start to first answer, one machine

The create times above are not comparable with the baselines' start times: those start one instance at a time and poll every 50 ms, while `sizes` starts five at once per node and polls every 0.25 s, twice over. [`starttime.py`](../../starttime.py) times Sandstorm the baselines' way, and `baselines.py` ran again beside it, on bm (AMD EPYC 9K65, 2 vCPU, 3.6 GB; a faster CPU than the fleet nodes, so these are not the fleet's numbers), with `sandstormd` built from dev at 872682b, which has #360 and #361, and runsc `release-20261019.0`. Twenty of each, p50, in `starttime/`:

| | web | app64 |
|---|---:|---:|
| plain process | 52 ms | 103 ms |
| bubblewrap | 52 ms | 103 ms |
| Docker (runc) | 192 ms | 244 ms |
| Docker + gVisor | 217 ms | 333 ms |
| Sandstorm, master and node on bm | 167 ms | 289 ms |
| Sandstorm, warm template | 159 ms | 168 ms |

Sandstorm's time, from the node's log: master and network 36–37 ms, bundle 5 ms, `runsc create` and `start` 55–61 ms, then the app: 67 ms for web, 189 ms for app64, most of it importing numpy and filling the heap. A warm template restores the seed's image in 67–89 ms instead and answers 34–43 ms later. The warm runs did not reseed (`after_restore` empty), which a real template must.

CRIU under Podman, ten app64 per node: checkpoint 0.29–0.33 s p50, restore to first answer 0.24 s.

Docker's daemons are up only while Docker is measured and counted against it. Sandstorm's lead at the larger heaps is zswap: the node squeezes idle pages of sandboxes it keeps hot, and `numpy.ones` compresses far better than a real heap would.

## Bugs the runs turned up

All fixed in [#360](https://github.com/deeplethe/sandstorm/pull/360) unless said otherwise.

1. **A wake refused instead of making room.** The 13–33% above. A wake now puts sandboxes to disk first: the longest-idle at #360, the least in demand since #361.
2. **An eviction the master did not hear about was undone.** A report's observed tiers and its eviction list were read apart; one eviction fell between, and the master restored the sandbox 0.3 s after it went to disk. Evictions are now reported on every report for 15 s, and a restore of one is deferred meanwhile.
3. **Making room was too timid.** It first spared anything that had answered in the last 15 s, which at 4 req/s was nearly everything hot; #360 made it 2 s, and #361 spares a sandbox only while a request to it is in flight.
4. **Memory pressure refused wakes the node's own restores had caused.** #360 let a wake wait up to 8 s for it to ease; #361 bounds the whole wake, pressure included, to 2 s.
5. **A held-back create was quarantined.** Counted as a failure, five hold-backs and the master stopped asking: 1,246 and 1,565 of 20,000 never started. Now reported as held back, never counted, retried within 5 s.
6. **An error outlived the sandbox coming up.** `last_error` now clears when a report shows the sandbox up.
7. **A sandbox that died kept its network, and its slot went to the next one.** A sandbox killed by the OOM killer left a bundle with nothing running, which a node does not report, so the master never asked for it to be destroyed; it handed the slot on, both veths held the same address, and the new sandbox ran for fifteen minutes unreachable. The node now clears away a sandbox dead for a minute, network included, and a slot's setup removes any veth still holding its address.
8. **A machine that ran a forkd node kept forkd's iptables rules**, which reject the addresses Sandstorm hands out. Upgrade note in the CHANGELOG.
9. Not fixed: a few runsc containers in the `stopped` state that `runsc delete` cannot remove, because the gVisor filestore it wants to delete is already gone. Six across the fleet after these runs; they hold no memory.

The harness had its own: `docker rm` refuses Podman's `-t`, so the first baseline round compared everything against a machine still holding its Docker containers. Those runs were discarded; every row in `baselines/` comes from a machine that started with over 1.1 GB available.

## Running it again

```bash
scripts/fleet.sh up --master f1 --nodes f3,f5,f7,f8,f9,f10 --listen 80 --port 80
bench/fleet/apps.sh
# on each node, from a directory with fleet/{workloads,loadtest}.py, ca.pem and tok:
bash suite-node.sh <node-id> https://<master>:80 2
# on the master:
bash suite-master.sh https://<master>:80 <nodes> 2 walls
bash suite-master.sh https://<master>:80 <nodes> 2 timers 5 15
bash suite-master.sh https://<master>:80 <nodes> 2 fill 1
# then, on each node: workloads.py --node <id> probe --count 100 --tokens tokens-1.jsonl
```
