# Benchmarks

[`fleet/`](fleet/README.md) is the v0.6 load test: it drives a master
over its HTTP API, concurrently, across every node in a pool. The
numbers in the top-level [README](../README.md) come from
[`fleet/results/`](fleet/results/).

The v0.5 single-host benchmarks -- the N=100 spawn comparison against
Docker, gVisor, Firecracker, CubeSandbox, boxlite and OpenSandbox, the
pause-window and chain-spawn probes -- measured the Firecracker runtime
and drove its `sandstorm` CLI. They are in the
[`v0.5.3` tag](https://github.com/deeplethe/sandstorm/tree/v0.5.3/bench).
