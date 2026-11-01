# Creates side by side, 2026-10-31

A node used to start the sandboxes of a batch one after another. This compares that with starting them side by side, on one build, with the only difference the number of workers.

## How it was run

- **Machine:** the bare-metal machine of [2026-10-30](../2026-10-30/README.md): 16 threads, 31 GB, KVM. Firecracker on its XFS loop file, gVisor beside it, one at a time.
- **Harness:** [`fork.py`](../../fork.py) `--widths 10,20,50,100`, on `app64-warm`. Fork n is one call with n children, create n is n creates at once; both are timed until every sandbox answers, polled every 5 ms, three rounds. Sandboxes stay hot.
- **Serial:** this branch with `create_workers()` returning 1, so it has the same fixes and the same namespace pool of 32.
- **Parallel:** this branch at one worker per CPU, 16 here, with a pool of 32 (`compare.jsonl`) and of 128 (`pool.jsonl`).
- **Final:** this branch as it ships, with the pool at its default, 124 on this machine (`final.jsonl`). Two runs of each; the table has the mean of the two medians.

## Results

Milliseconds until all answer.

| Firecracker | 1 | 10 | 20 | 50 | 100 |
|---:|---:|---:|---:|---:|---:|
| fork, serial | 111 | 542 | 1,016 | 3,135 | 8,021 |
| fork, parallel, pool of 32 | 115 | 192 | 291 | 2,309 | 7,900 |
| **fork, final** | **108** | **196** | **343** | **783** | **1,664** |
| create, serial | | 546 | 1,070 | 3,129 | 7,930 |
| **create, final** | | **232** | **407** | **784** | **1,634** |

| gVisor | 1 | 10 | 20 | 50 | 100 |
|---:|---:|---:|---:|---:|---:|
| fork, serial | 117 | 996 | 2,141 | 6,254 | 13,837 |
| fork, parallel, pool of 32 | 120 | 425 | 816 | 3,154 | 7,776 |
| **fork, final** | **128** | **509** | **916** | **2,086** | **3,667** |
| create, serial | | 2,979 | 5,907 | 15,310 | 31,772 |
| **create, final** | | **2,056** | **3,658** | **7,916** | **15,909** |

A snapshot took 18 ms on Firecracker and 383 ms on gVisor in the final runs, so a clone into 20 is about 361 ms on Firecracker. CubeSandbox on this machine, [a day earlier](../2026-10-30/README.md): 973-1,046 ms into 10, 1,046-1,064 ms into 20, its own SDK's `clone(n, concurrency=n)`. Past 20 it was not run.

- **One sandbox pays nothing.** A fork of one is within noise of serial.
- **Past the pool, parallel gains nothing on Firecracker.** With 32 kept built, 100 took as long side by side as one after another: the rest each build a namespace, and that work waits on the kernel's RTNL lock. With the pool at 128 it was 1.6 s, so the pool now follows memory.
- **The gVisor create rows are too slow, and not because of creates.** This machine's warm image had gone stale under a host upgrade, so every sandbox died and started again cold. See [the warm image run](../2026-10-31-warm-image/README.md): with a new image, create 100 took 3,820 ms against the 15,909 here. The forks here are unaffected, and so is Firecracker.

## On 2-core nodes

Six nodes of 2 vCPU and 1.9 GB, gVisor, through `loadtest.py`. A node has 2 workers. `fleet/` has the lines below.

| 64 at a time | Before | After |
|---|---:|---:|
| 900 `app1024`, up after 60 s | 12 | 12 |
| Lowest memory available on any node | 161 MiB | 195 MiB |
| Slowest of about 390 ssh checks, none failed | 1.6 s | 1.0 s |
| 240 `web`, up after 10 s, two runs | 109, 111 | 103, 97 |
| 240 `web`, up after 60 s, two runs | 205, 208 | 198, 197 |

"Before" for `app1024` is the build the fleet was already on, for `web` the serial build above. So a node at its memory floor gets there about 5% slower: a create is counted for its whole limit from the moment it is let through, which is stricter than the look at what it has used. A machine this small gains little from this change, and bursts that fill it are safe.

## A bug this found

100 at a time left one child unreachable for good, on the serial build as well. The master repeats a destroy until a report drops the sandbox, and 52 of 943 destroys were repeated after the first had finished. A sandbox past the pool has a veth under its own name, which by then had gone back to the pool; the repeat took it down or deleted it, by name. A later claim failed with `No such device` after the guest was running, so the node reported it running and the master never asked again. Both halves are fixed. The first has a test that fails without the fix; the link between it and the failure was read from the code and the logs, not watched happening.
