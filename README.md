<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/logo-dark.svg">
  <img alt="Sandstorm" src="docs/logo.svg" width="383">
</picture>

</div>

<br/>

<div align="center">

[Sandbox lifecycle](#adaptive-sandbox-lifecycle) · [Key features](#key-features) · [Latency](#latency) · [Quick start](#quick-start) · [Status](#status)

[![CI](https://img.shields.io/github/actions/workflow/status/deeplethe/sandstorm/ci.yml?branch=dev&style=flat-square&label=CI&color=0B7285&labelColor=161B22)](https://github.com/deeplethe/sandstorm/actions)
[![Rust](https://img.shields.io/badge/BUILT%20WITH-RUST-F74C00?style=flat-square&labelColor=161B22&logo=rust&logoColor=FFFFFF)](https://www.rust-lang.org)
[![gVisor](docs/assets/badges/gvisor.svg)](https://gvisor.dev)
[![Firecracker](docs/assets/badges/firecracker.svg)](https://firecracker-microvm.github.io)
[![License](https://img.shields.io/badge/LICENSE-APACHE%202.0-2D333B?style=flat-square&labelColor=161B22)](LICENSE)
[![DeepLethe](https://img.shields.io/badge/BUILT%20BY-DEEPLETHE-F1F2F5?style=flat-square&labelColor=161B22)](https://deeplethe.com)
[![中文](https://img.shields.io/badge/LANG-%E4%B8%AD%E6%96%87-DA3633?style=flat-square&labelColor=161B22)](README-zh.md)

</div>

**One machine, thousands of sandboxes.** Sandstorm pauses idle sandboxes and snapshots them to disk, then restores them quickly when a request arrives, so one machine can hold far more sandboxes than it could run at once. It supports two sandbox backends, [gVisor](https://gvisor.dev) containers and [Firecracker](https://firecracker-microvm.github.io) microVMs. Every sandbox has its own network namespace, and both backends can run in one fleet, each forming a resource pool of its own. The master serves a web console for viewing and managing sandboxes and scheduling across the fleet.

<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/charts/compare-dark.svg">
  <img alt="Start time, memory and clone time of a 64 MiB app, by runtime: on a cloud VM, Sandstorm on Firecracker from a warm template 84 ms and CubeSandbox 87 ms; on a KVM machine 100 ms and 129 ms, in 11 and 12 MiB, and 938 ms and 1.1 s to clone into 20; Docker with gVisor 685 ms, microsandbox 1.1 s and Kata 3.4 s." src="docs/assets/charts/compare-light.svg" width="100%">
</picture>

</div>

<sub>The 64 MiB app, p50 of 20 started one at a time, log scales. Each panel is ranked on its own, best first, in gold; Sandstorm's other rows are in blue. FC: [Firecracker](https://firecracker-microvm.github.io). Cube: [CubeSandbox](https://github.com/TencentCloud/CubeSandbox). Also shown: [microsandbox](https://github.com/superradcompany/microsandbox), [Kata](https://katacontainers.io), [Docker](https://www.docker.com), [Podman](https://podman.io), [bubblewrap](https://github.com/containers/bubblewrap). Warm: restored from an image of the app already started. Cloud VM: six 2-core, 2 GB nodes for [gVisor](https://gvisor.dev) and containers; one 2-core, 3.6 GB VM under [PVM](https://github.com/virt-pvm/linux) for Firecracker and CubeSandbox. KVM machine: 2 × Xeon E5-2637 v2, 31 GB. Clones are of a running app, until all 20 answer (microsandbox: until running).</sub>

## Adaptive Sandbox Lifecycle

Agents spend much of their time waiting. Sandstorm is designed around this behavior, with four sandbox tiers that progressively suspend idle sandboxes and quickly wake them when requests arrive.

| Tier | State | Memory | Back to hot |
|---|---|:---:|---:|
| **hot** | Running | Held | — |
| **frozen** | Paused, cold pages in zswap | Partly held | ~0.1 s |
| **snapshot** | Checkpointed to disk | Freed | ~0.2 s |
| **detached** | Only the volume remains | Freed | Cold start |

## Key Features

- **One master, many nodes.** The master keeps desired state in SQLite and places sandboxes. Nodes join by address and pairing code, or dial out from behind NAT.

- **Wake on traffic.** A gateway on the sandbox's node wakes it and forwards the request.

- **Node self-protection.** Nodes refuse new work when memory runs low, so SSH stays reachable.

- **One binary.** `sandstormd` serves as the master, node, and web console.

- **Tuned for each backend.** gVisor and Firecracker each get [their own optimizations](docs/OPTIMIZATIONS.md).

The master's web console shows sandbox states, tier transitions, and node headroom.

<p align="center">
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/console-dark.webp">
  <img alt="The Sandstorm console: counts of sandboxes, awake ones, nodes, creates per minute and API p99 across the top; the tier flow between awake, frozen, on disk and detached; sandboxes by tier over time, lifecycle rates, node headroom and latency." src="docs/assets/console-light.webp">
</picture>
</p>

## Latency

Measured 2026-10-31, with requests from each node to its own gateway, so no network is in the path. gVisor needs no KVM and ran on six cloud nodes. Firecracker ran on one machine with KVM and one cloud VM under PVM. The hosts differ, so read each column on its own.

| | gVisor | Firecracker, KVM | Firecracker, PVM |
|---|---:|---:|---:|
| Host | 2 cores, 2 GB | 16 threads, 31 GB | 2 vCPU, 3.8 GB |
| Wake from frozen, 64 MiB app | 100 ms | 165 ms | 78 ms |
| Wake from disk, 16 / 64 / 256 MiB app | 130 / 150 / 720 ms | 150 / 150 / 500 ms | 80 / 135 / 450 ms |
| Wake from disk, 512 MiB / 1 GiB app | 1.1 / 3.3 s | 0.7 / 1.4 s | 0.4 s / not run |
| Kept in memory per node: web / 64 MiB / 256 MiB app | 76 / 18 / 6 | 293 / 160 / 85 | 45 / 34 / 18 |
| Refused at 1, 2, 4, 8 requests a second per node | 2.1 / 0.2 / 0.1 / 1.6% | 0.2 / 0 / 0 / 0% | 0 / 0 / 0 / 0% |
| Answered, of 10 / 40 woken at the same instant | 6 / 6 | 10 / 28 | 5.5 of 8 |

Traffic is 40 sandboxes per node with a few getting most requests; about 12 fit awake on a gVisor node. p50 is 6-10 ms on gVisor, 7-50 ms on KVM and 4-14 ms on PVM; p99 is about 2 s, 0.8-1.9 s and 1.8-3.6 s, the requests that had to wait for a restore. A wake that cannot start within 2 s is refused rather than queued. A gVisor node here restores two at a time and queues four, so of 10 or more woken at once, six are answered and the rest get a 503 at once.

The "kept in memory" metric counts sandboxes that remained running with swap enabled; it does not indicate how quickly they can respond to requests.

Without swap, the PVM host could accommodate only about eight instances of the 64 MiB app. As a result, the traffic test used a smaller fleet of 20 sandboxes, and the 1 GiB app was not tested.

Firecracker's results for the 512 MiB and 1 GiB configurations are based on a single sample each.

See the [full results and testing methodology](bench/fleet/results/2026-10-31/README.md).

## Method

The figure at the top: every runtime started one at a time, polled every 5 ms, p50 of 20 ([`compare.py`](bench/baremetal/compare.py)). Sandstorm's time includes the master placing the sandbox and the node setting up its network. The cloud column is the median of six 2-core, 2 GB nodes, and for Firecracker and CubeSandbox one 2-core, 3.6 GB VM with CubeSandbox's PVM host kernel, which gives a VM without KVM a `/dev/kvm`.

A warm template restores an image of the app already started, as CubeSandbox always does; it is opt-in here, because every sandbox starts from the same memory and the template has to say how to reseed. CubeSandbox's API always restores a snapshot, so its cold figure comes from its own `cubemastercli multirun`, with a template's create request minus the snapshot annotations and a 5 ms probe: from the client's call until the program answers, as for the rest.

Under PVM the two warm starts are level: Sandstorm 52 / 83 ms, CubeSandbox 53 / 87 ms for web and the 64 MiB app. On the KVM machine Sandstorm's is ahead: 67 / 100 ms against 78 / 129 ms. Cold, Sandstorm leads on both: 545 / 921 ms against 828 / 999 ms on the KVM machine, 667 / 869 ms against 845 / 1,094 ms under PVM, where a guest boots without a serial console unless `--fc-serial-console` says otherwise. The 64 MiB app was 74 ms and 78 ms on the same machine a day earlier, with the same binary, so take that column as relative. A snapshot of a running sandbox took Sandstorm 30 ms and CubeSandbox 304 ms. Clones into 1, 10 and 20 took Sandstorm 126, 214 and 361 ms, and CubeSandbox 494, 1,046 and 1,064 ms; Sandstorm's 100 took 1.7 s, and CubeSandbox was not run past 20. E2B self-hosted was not set up, and memory under PVM was not measured. Raw rows: [bench/baremetal/results/2026-10-30](bench/baremetal/results/2026-10-30/README.md), [bench/fleet/results/2026-10-30](bench/fleet/results/2026-10-30/README.md); Sandstorm's clones, a day later with parallel creates: [bench/baremetal/results/2026-10-31-parallel](bench/baremetal/results/2026-10-31-parallel/README.md).

## Quick start

You need Linux with cgroup v2, [`runsc`](https://gvisor.dev/docs/user_guide/install/) on the `PATH`, and Rust to build.

```bash
git clone https://github.com/deeplethe/sandstorm.git && cd sandstorm
cargo build --release -p sandstorm-core
sudo ./target/release/sandstormd serve --master --node
```

That runs a master and a node in one process, with the console at `https://127.0.0.1:8080`. The API token is in `/var/lib/sandstorm/pki/api-token`. A sandbox starts from a template, a rootfs plus an OCI spec registered with `POST /v1/templates`; `scripts/fleet.sh seed` shows the smallest one.

**A fleet** of machines you can ssh to:

```bash
scripts/fleet.sh up --master m1 --nodes n1,n2,n3 --listen 80
scripts/fleet.sh seed     # put a template on each node
scripts/fleet.sh verify   # start one sandbox and check it runs
```

A client on another machine verifies the name it connects to, and the master's certificate names only `127.0.0.1`, `localhost` and its `--bind` address. Add yours with `--tls-san master`, which can be repeated.

**A node that is already installed** listens with `SANDSTORM_LISTEN=0.0.0.0:80`. On first start it writes a pairing code to `/var/lib/sandstorm/pki/node/pairing-code`. Give the master its address and that code, with the master's CA and API token from its `/var/lib/sandstorm/pki`:

```bash
curl --cacert ca.pem -H "Authorization: Bearer $(cat api-token)" -H 'Content-Type: application/json' \
  https://master:8080/v1/nodes -d '{"address": "10.0.0.5", "pairing_code": "<code>"}'
```

The code stays the same across restarts, and whoever holds it can run anything on that node. Keep it like a root password.

The HTTP API is documented in [docs/API.md](docs/API.md). To load-test a fleet, use [bench/fleet](bench/fleet/README.md).

## Status

v0.6 is not released yet, and it changes a lot: the backend moves from Firecracker microVMs to gVisor, and one host becomes a fleet. The last release, v0.5.3, is the Firecracker fork runtime; its README is [at that tag](https://github.com/deeplethe/sandstorm/tree/v0.5.3). The design decisions are recorded in [docs/adr](docs/adr/README.md).

Next, from what the fleet runs found:

- [x] Make room on wake: put the sandbox least in demand to disk instead of refusing the request.
- [x] Retry a create the node held back once memory frees, instead of parking it; and clear its error once it runs.
- [x] Clear away a sandbox that dies, network included, and restart it if it is still wanted.
- [ ] Reserve capacity on the master as it places, so a burst cannot overshoot a node's last report.
- [ ] Restore several snapshots in parallel on one node.

## License

[Apache-2.0](LICENSE)
