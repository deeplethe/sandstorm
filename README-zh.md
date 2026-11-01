<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/logo-dark.svg">
  <img alt="Sandstorm" src="docs/logo.svg" width="383">
</picture>

</div>

<br/>

<div align="center">

[沙箱生命周期](#自适应沙箱生命周期) · [核心特性](#核心特性) · [延迟](#延迟) · [快速开始](#快速开始) · [状态](#状态)

[![CI](https://img.shields.io/github/actions/workflow/status/deeplethe/sandstorm/ci.yml?branch=dev&style=flat-square&label=CI&color=0B7285&labelColor=161B22)](https://github.com/deeplethe/sandstorm/actions)
[![Rust](https://img.shields.io/badge/BUILT%20WITH-RUST-F74C00?style=flat-square&labelColor=161B22&logo=rust&logoColor=FFFFFF)](https://www.rust-lang.org)
[![gVisor](docs/assets/badges/gvisor.svg)](https://gvisor.dev)
[![Firecracker](docs/assets/badges/firecracker.svg)](https://firecracker-microvm.github.io)
[![License](https://img.shields.io/badge/LICENSE-APACHE%202.0-2D333B?style=flat-square&labelColor=161B22)](LICENSE)
[![DeepLethe](https://img.shields.io/badge/BUILT%20BY-DEEPLETHE-F1F2F5?style=flat-square&labelColor=161B22)](https://deeplethe.com)
[![English](https://img.shields.io/badge/LANG-ENGLISH-2D333B?style=flat-square&labelColor=161B22)](README.md)

</div>

**一台机器，万千沙箱。** Sandstorm 通过暂停闲置沙箱并将其快照保存到磁盘，在请求到达时快速恢复，使单台机器能够承载远超其实际并发运行能力的沙箱数量。支持 [gVisor](https://gvisor.dev) 容器和 [Firecracker](https://firecracker-microvm.github.io) microVM 两种沙箱后端。每个沙箱都拥有独立的网络命名空间，不同后端可以在同一集群中运行，并形成各自独立的资源池。主控节点提供 Web 控制台，用于查看和管理沙箱及集群调度。

<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/charts/compare-dark.svg">
  <img alt="各运行时 64 MiB 应用的启动时间、内存和克隆时间：云主机上 Sandstorm Firecracker 预热模板 84 ms，CubeSandbox 87 ms；KVM 机器上 100 ms 和 129 ms，内存 11 和 12 MiB，克隆成 20 份 938 ms 和 1.1 s；Docker + gVisor 685 ms，microsandbox 1.1 s，Kata 3.4 s。" src="docs/assets/charts/compare-light.svg" width="100%">
</picture>

</div>

<sub>64 MiB 应用，启动 20 次取 p50，对数坐标。每栏各自排名，第一名是金色；Sandstorm 的其他行是蓝色。FC：[Firecracker](https://firecracker-microvm.github.io)。Cube：[CubeSandbox](https://github.com/TencentCloud/CubeSandbox)。另有 [microsandbox](https://github.com/superradcompany/microsandbox)、[Kata](https://katacontainers.io)、[Docker](https://www.docker.com)、[Podman](https://podman.io)、[bubblewrap](https://github.com/containers/bubblewrap)。warm：从应用已启动好的镜像恢复。云主机：[gVisor](https://gvisor.dev) 和容器用 6 台 2 核 2 GB 节点；Firecracker 和 CubeSandbox 用一台 [PVM](https://github.com/virt-pvm/linux) 下的 2 核 3.6 GB VM。KVM 机器：2 × Xeon E5-2637 v2、31 GB。克隆指运行中的应用，到 20 份全部响应为止（microsandbox 到运行为止）。</sub>

## 自适应沙箱生命周期

Agent 大部分时间都处于等待状态。Sandstorm 针对这一特点设计了四级运行状态，让闲置沙箱逐级休眠，并在请求到达时快速唤醒。

| 层级 | 状态 | 内存 | 恢复至 hot |
|---|---|:---:|---:|
| **hot** | 正常运行 | 占用 | — |
| **frozen** | 暂停，冷数据页存入 zswap | 部分占用 | 约 0.1 秒 |
| **snapshot** | 快照保存至磁盘 | 释放 | 约 0.2 秒 |
| **detached** | 仅保留数据卷（Volume） | 释放 | 冷启动 |

## 核心特性

- **主从架构。** 一个主控节点管理多个工作节点，使用 SQLite 保存期望状态并负责沙箱调度。节点可通过地址和配对码接入，也可以主动连接主控节点以适应 NAT 网络。

- **流量唤醒。** 请求到达节点网关后，自动唤醒对应沙箱并转发请求。

- **节点自我保护。** 内存不足时自动拒绝新任务，避免资源耗尽，同时保持 SSH 可用。

- **单一二进制。** `sandstormd` 同时支持主控节点、工作节点和 Web 控制台。

- **按后端优化。** gVisor 和 Firecracker 各有[专门的优化](docs/OPTIMIZATIONS.md)。

主控节点提供 Web 控制台，可查看沙箱状态、各层级之间的切换情况，以及节点的资源余量。

<p align="center">
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/console-dark.webp">
  <img alt="Sandstorm 控制台：顶部是沙箱数、常驻数、节点数、每分钟创建数和 API p99；左边是常驻、冻结、磁盘、离线四档之间的流动；右边是各档沙箱数随时间变化、生命周期速率、节点余量和延迟。" src="docs/assets/console-light.webp">
</picture>
</p>

## 延迟

测于 2026-10-31，请求从每个节点发给它自己的 gateway，路径上没有网络。gVisor 不需要 KVM，跑在 6 台云主机上；Firecracker 分别跑在一台有 KVM 的机器和一台 PVM 云主机上。各环境硬件不同，请按列单独看。

| | gVisor | Firecracker，KVM | Firecracker，PVM |
|---|---:|---:|---:|
| 主机 | 2 核 2 GB | 16 线程 31 GB | 2 vCPU 3.8 GB |
| 从冻结唤醒，64 MiB 应用 | 100 ms | 165 ms | 78 ms |
| 从磁盘唤醒，16 / 64 / 256 MiB 应用 | 130 / 150 / 720 ms | 150 / 150 / 500 ms | 80 / 135 / 450 ms |
| 从磁盘唤醒，512 MiB / 1 GiB 应用 | 1.1 / 3.3 s | 0.7 / 1.4 s | 0.4 s / 未测 |
| 每节点常驻内存：web / 64 MiB / 256 MiB 应用 | 76 / 18 / 6 | 293 / 160 / 85 | 45 / 34 / 18 |
| 每节点每秒 1、2、4、8 个请求时的拒绝率 | 2.1 / 0.2 / 0.1 / 1.6% | 0.2 / 0 / 0 / 0% | 0 / 0 / 0 / 0% |
| 同时唤醒 10 / 40 个时的成功数 | 6 / 6 | 10 / 28 | 8 个中 5.5 |

流量测试为每节点 40 个沙箱，少数沙箱拿到大部分请求；一个 gVisor 节点只放得下约 12 个醒着的。p50 在 gVisor 上是 6-10 ms，KVM 上是 7-50 ms，PVM 上是 4-14 ms；p99 分别约 2 s、0.8-1.9 s 和 1.8-3.6 s，是需要等恢复的请求。2 秒内起不来的唤醒会被拒绝，而不是排队。这里的 gVisor 节点同时恢复 2 个、排队 4 个，所以同时唤醒 10 个及以上时，6 个得到响应，其余立刻收到 503。

「内存驻留数量」（Kept in memory）这一指标统计的是在启用 Swap 的情况下，仍然保持运行的沙箱数量，而不是沙箱能够多快地响应请求。

在不启用 Swap 的情况下，PVM 主机的内存最多只能容纳约 8 个运行 64 MiB 应用的沙箱。因此，实际流量测试采用了较小的集群规模，共运行 20 个沙箱，且没有测试 1 GiB 的应用。

Firecracker 在 512 MiB 和 1 GiB 两种内存配置下的测试结果，均仅基于一次采样。

查看[完整测试数据及测试方法](bench/fleet/results/2026-10-31/README.md)

## 测试方法

顶部的图：每种运行时一次启动一个，每 5 ms 请求一次，测 20 次取 p50（[`compare.py`](bench/baremetal/compare.py)）。Sandstorm 的时间包括 master 调度和节点建网络。云主机一栏是 6 台 2 核 2 GB 节点的中位数；Firecracker 和 CubeSandbox 是一台 2 核 3.6 GB 的 VM，用 CubeSandbox 的 PVM 内核，让没有 KVM 的 VM 也有 `/dev/kvm`。

预热模板恢复的是应用已经启动好的镜像，CubeSandbox 一直是这样启动的；Sandstorm 这边需要主动开启，因为所有沙箱都从同一份内存开始，模板得说明怎么重新播种随机数。CubeSandbox 的 API 始终从快照恢复，所以它的 cold 数字来自它自己的 `cubemastercli multirun`：用模板的创建请求，去掉快照注解，探针 5 ms 一次，从客户端调用到程序响应，和其他行口径一致。

PVM 上两边的 warm 启动基本持平：web 和 64 MiB 应用，Sandstorm 是 52 / 83 ms，CubeSandbox 是 53 / 87 ms。KVM 机器上 Sandstorm 领先：67 / 100 ms 对 78 / 129 ms。cold 启动 Sandstorm 都领先：KVM 机器上 545 / 921 ms 对 828 / 999 ms，PVM 上 667 / 869 ms 对 845 / 1,094 ms。PVM 上 guest 默认不开串口，需要时用 `--fc-serial-console` 打开。同一台机器、同一个二进制，一天前 64 MiB 应用是 74 ms 和 78 ms，所以这一栏看相对差距。给运行中的沙箱打快照，Sandstorm 要 30 ms，CubeSandbox 要 304 ms。克隆成 1、10、20 份，Sandstorm 是 126、214、361 ms，CubeSandbox 是 494、1,046、1,064 ms；Sandstorm 克隆 100 份用 1.7 s，CubeSandbox 没有测过 20 份以上。E2B 自托管没有搭，PVM 上的内存没有测。原始数据见 [bench/baremetal/results/2026-10-30](bench/baremetal/results/2026-10-30/README.md)、[bench/fleet/results/2026-10-30](bench/fleet/results/2026-10-30/README.md)；Sandstorm 的克隆数据是一天后并行创建的结果：[bench/baremetal/results/2026-10-31-parallel](bench/baremetal/results/2026-10-31-parallel/README.md)。

## 快速开始

需要 Linux 和 cgroup v2，`PATH` 里要有 [`runsc`](https://gvisor.dev/docs/user_guide/install/)，编译需要 Rust，面板需要 Node 22.12 或更新版本。

```bash
git clone https://github.com/deeplethe/sandstorm.git && cd sandstorm
npm --prefix web/console ci && npm --prefix web/console run build
cargo build --release -p sandstorm-core
sudo ./target/release/sandstormd serve --master --node
```

这样会在一个进程里同时跑 master 和节点。打开面板 `https://127.0.0.1:8080`，浏览器会提示证书来自私有 CA，继续访问；用 `/var/lib/sandstorm/pki/api-token` 里的 token 登录，点 **+ From an image** 建一个模板，再从它启动沙箱。

**多台能 ssh 上去的机器组成集群：**

```bash
scripts/fleet.sh up --master m1 --nodes n1,n2,n3 --listen 80
scripts/fleet.sh seed     # 在每个节点放一个模板
scripts/fleet.sh verify   # 启动一个沙箱，确认真的能跑
```

其他机器上的客户端，要用 `--tls-san` 把它连接时用的名称加进 master 的证书。已经装好的节点，或者在 NAT 后只能主动连 master 的节点，按 [API 文档](docs/API.md#nodes)加入。面板能做的事都在 [HTTP API](docs/API.md) 里，集群压测见 [bench/fleet](bench/fleet/README.md)。

## 状态

v0.6 是当前版本，改动很大：gVisor 成为默认后端，节点不再需要 KVM；Firecracker 保留为第二后端，按节点选择；从单机变成集群。上一个版本 v0.5.3 是基于 Firecracker 的 fork 运行时，它的 README 见[那个 tag](https://github.com/deeplethe/sandstorm/tree/v0.5.3)。架构决策记录在 [docs/adr](docs/adr/README.md)（英文）。

接下来，来自集群实测发现的问题：

- [x] 唤醒时腾出位置：把最少被用到的沙箱落盘，而不是拒绝请求。
- [x] 节点暂缓的创建，在内存空出来后重试，而不是停放；跑起来后清除它的错误状态。
- [x] 清理死掉的沙箱，连同它的网络；如果还需要它，就重新启动。
- [ ] master 在分配时预留容量，避免一波突发请求超过节点上次上报的余量。
- [x] 同一节点上并行恢复多个 snapshot。

## 许可证

[Apache-2.0](LICENSE)
