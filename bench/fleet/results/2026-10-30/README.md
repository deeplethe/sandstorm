# Fleet run, 2026-10-30

The six nodes of [2026-10-28](../2026-10-28/README.md) (f3, f5, f7, f8, f9, f10; master f1), rebooted first, on `dev` with #376, #380 and #385: build **v13**. `suite-node.sh` on every node at once with the same parameters, then the hot walls. Idle timers: frozen after 15 s, on disk after 60 s.

Not rerun: the 20,000 fills and `heaps/`. A heaps wall of 900 `app1024` in 2 s, on four nodes because two had not yet reconnected, took those nodes down: admission counted no sandbox that had not yet filled its heap. Fixed in #387. Its first attempt, with 15/60 s timers, measured nothing useful either: sandboxes went to disk during the 90 s settle.

## Under traffic

Zipf 1.1, open loop, 150 s at each rate per node.

| Per node | 40: refused | p50 | p99 | 80: refused | p50 | p99 |
|---|---:|---:|---:|---:|---:|---:|
| 1 req/s | 1.3% | 7.8 ms | 2.4 s | 2.1% | 28.4 ms | 2.8 s |
| 2 req/s | 0.2% | 6.4 ms | 1.7 s | 0.2% | 11.7 ms | 2.1 s |
| 4 req/s | 0.1% | 5.7 ms | 1.7 s | 0.2% | 9.1 ms | 2.1 s |
| 8 req/s | 1.3% | 6.7 ms | 2.2 s | 8.2% | 9.4 ms | 3.2 s |

40,523 and 27,213 requests. Nearly all refusals are the 503 of a wake that could not start within 2 s.

## Wake and restore

| | |
|---|---:|
| Frozen, 64 MiB app (25-50 s idle), p50 by idle time | 100-170 ms |
| From disk, 64 MiB app (80-170 s idle), p50 by idle time | 160-180 ms |
| Restore p50: 16 / 64 / 256 / 512 MiB / 1 GiB | 129 / 148 / 753 / 1,063 / 3,344 ms |

Up to 14 s idle, more sandboxes than on 2026-10-28 needed a wake of about 300 ms: 17-50% answered at once, against 75%. A bug, fixed in #401: a program still starting refuses connections, the gateway took that for a cold sandbox, and under the memory floor the wake put two others to disk for a sandbox that needed nothing. Each of those then did the same to the next. On 2026-10-28 this was already there: 9 runs of 12 had no wake at all and 3 woke nearly every sandbox, and those averaged to 75%. With the fix, f3 ran the same 14 sandboxes twice: no wakes and no evictions, against 7 wakes and 9 evictions before it, and 2.8 ms instead of 342 ms after 1 s idle. The fleet's timers were at 3600 / 7200 s for those runs, so the rows for frozen and disk above are unchanged.

## Herds

Of K woken at the same instant on a node, 1 and 4 all answer; of 10, 20 or 40, six do, within 1.6 s, and the rest get a 503 at once. Two restores at a time and four waiting (#361, which came after the 2026-10-28 herds).

## Kept in memory, per node

web 50.6, app16 22.8, app64 13.1, app256 6.0: `v13/walls`, two runs each, 10 s settle.
