# Fleet load test

`loadtest.py` drives a v0.6 master over its HTTP API, concurrently,
across every node registered in a pool, and reads `/metrics` either side
of the run so the control plane's own view sits beside the client's.

The v0.5 benchmarks (in the
[`v0.5.3` tag](https://github.com/deeplethe/sandstorm/tree/v0.5.3/bench))
measured one host: `bench-spawn-100.sh` spawned 100 sandboxes through
the old single-host CLI, and the `pause-window` and `chain-spawn` probes
drove the HTTP API one sandbox at a time. Neither shape answers what a
fleet raises: how creates distribute, where the master starts refusing,
and whether the thing in the way is a node or the master's own lock.

Stdlib only. No build step, nothing to install.

```bash
curl -k https://master:8080/ca.pem -o ca.pem     # verify the fingerprint first

python3 bench/fleet/loadtest.py \
  --master https://master:8080 --ca ca.pem \
  --token-file /var/lib/sandstorm/pki/api-token \
  --total 500 --concurrency 32 --hold-secs 120 --touch \
  --out run.json
```

## What it does

| phase | |
|---|---|
| preflight | checks `/healthz`, lists nodes **grouped by pool**, refuses to start if the fleet already holds sandboxes it did not make (`--allow-existing`), takes a `/metrics` baseline |
| ramp | `--total` creates through `--concurrency` workers, optionally paced at `--rate`/s; records per-create latency, status and placement |
| hold | keeps the fleet up for `--hold-secs`, sampling tier counts and per-node memory/pressure every 5 s; `--touch` keeps sandboxes hot |
| teardown | deletes everything it made and checks the fleet came back to where it started (`--keep` to skip) |
| report | client-side percentiles, placement spread, refusal reasons, and the master's own numbers |

Nothing here touches a node directly. It creates sandboxes through the
API and deletes them again.

## Four things to know before reading a number out of it

**1. A mixed fleet splits itself into pools.** A node whose host
fingerprint differs from the pool's joins a *derived* pool named
`default#<fp8>`, because a runsc checkpoint does not restore across
differing kernels or CPUs. Give five differently-specced machines the
same `--pool default` and you get five pools, and a run against
`default` loads one of them. Preflight prints the grouping and says so;
`--pool` picks which one to drive.

**2. A sandbox falls to `frozen` after 30 s idle** (the master's
`--idle-frozen-secs`). A run that creates a few hundred and then
measures them is measuring restores, not steady state. `--touch` keeps
them hot; without it the report says how many drifted, so the number is
not quietly read as something else:

```
                 without --touch          with --touch
tiers at end     {'frozen': 20}           {'hot': 20}
master saw       20 demotions             0 demotions
```

**3. Placement by count is exact; admission by memory is not.**
The scheduler counts a node's sandboxes from the database under the same
lock it places under, so concurrent creates spread exactly — 90 creates
at concurrency 16 across three nodes came out 30/30/30. Free memory and
pressure are different: they come from the node's last poll, which is up
to `--hold-secs` old, and nothing is reserved against them. A burst
arriving inside one poll window all sees the same figure. At high
`--concurrency` that overshoot may be what you are measuring.

**4. A capacity refusal is a 503.** It means the fleet is full, not that
the master is broken. The ramp stops at the first one by default,
because that is the capacity answer; `--through-refusals` keeps going.

```
creates   120 placed, 15 refused, 0 failed
placement n1=40, n2=40, n3=40
refused   15x  every node in pool default is too loaded:
               n1: 40 sandboxes, at its cap of 40; n2: ...
```

## The master's own view

When the master serves `/metrics`, the report ends with numbers the
client cannot see:

```
store lock p50 0.3ms  p99 2.5ms  (held p99 0.5ms)
master saw 120 placed, 15 refused, 0 demotions
POST /v1/sandboxes server-side p99 4.1ms
```

`store lock` is the one to watch. One mutex guards one sqlite
connection, so that is where the master serialises. If create latency
climbs while *wait* stays flat, the queue is not there — look at the
nodes. If wait climbs and *held* does not, it is contention.

The lock quantiles are over the master process's whole life, not the
run's window — a counter difference is meaningful over a window, a
histogram's is not. Restart the master for a clean read.

`run.json` carries everything, including the raw `/metrics` deltas and
the per-sample tier and node timeline.
