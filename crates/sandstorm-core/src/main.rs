//! `sandstorm` — one binary, two modes.

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use sandstorm_core::identity::random_token;
use sandstorm_core::master::{self, MasterConfig};
use sandstorm_core::node::{self, NodeConfig, RuntimeConfig};
use sandstorm_core::state::Store;
use sandstorm_core::tier::IdlePolicy;

#[derive(Parser)]
#[command(
    name = "sandstormd",
    version,
    about = "sandbox fleet, oversold on purpose"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the master, a node, or both in one process.
    Serve(Box<ServeArgs>),
    /// Mint a single-use registration token for a new node.
    NodeToken(NodeTokenArgs),
    /// List registered nodes.
    Nodes(StateArgs),
    /// Check that this machine can run a backend, exactly as a node starting
    /// on it would, and exit non-zero saying why if it cannot. The installer
    /// runs it before it writes a unit, so a node that cannot run its
    /// backend is never installed.
    Preflight(PreflightArgs),
}

#[derive(Args)]
struct PreflightArgs {
    /// runsc (or gvisor) or firecracker.
    #[arg(long)]
    backend: String,
    #[arg(long, default_value = "/var/lib/sandstorm", env = "SANDSTORM_WORK_DIR")]
    work_dir: PathBuf,
    #[arg(long, default_value = "runsc")]
    runsc: String,
    #[arg(long, default_value = "firecracker")]
    firecracker: String,
    #[arg(long, default_value = "/var/lib/sandstorm/vmlinux")]
    fc_kernel: PathBuf,
}

#[derive(Args)]
struct ServeArgs {
    /// Run the control plane here.
    #[arg(long)]
    master: bool,
    /// Run the sandbox agent here.
    #[arg(long)]
    node: bool,

    #[arg(long, default_value = "127.0.0.1:8080", env = "SANDSTORM_BIND")]
    bind: SocketAddr,
    /// A name or address clients reach this master by, for its certificate.
    /// Repeat it, or separate them with commas in the environment variable.
    /// The certificate always names 127.0.0.1, localhost and --bind, and a
    /// client verifies the name it dialled, so one that comes by any other
    /// name is refused without this. That includes a node's --master-url:
    /// only its registration is pinned, and every later call verifies the name.
    #[arg(long = "tls-san", env = "SANDSTORM_TLS_SAN", value_delimiter = ',')]
    tls_san: Vec<String>,
    #[arg(
        long,
        default_value = "/var/lib/sandstorm/state.db",
        env = "SANDSTORM_STATE_DB"
    )]
    state_db: PathBuf,
    #[arg(
        long,
        default_value = "/var/lib/sandstorm/pki",
        env = "SANDSTORM_PKI_DIR"
    )]
    pki_dir: PathBuf,

    /// Master to dial. Defaults to this process's own bind address when
    /// --master is also set, which is what makes the single-machine case
    /// a one-flag deployment.
    #[arg(long, env = "SANDSTORM_MASTER_URL")]
    master_url: Option<String>,
    /// Wait here for a master to connect, instead of dialing one.
    ///
    /// The master is then given this address and the pairing code, which is
    /// written to `<pki-dir>/node/pairing-code` (root only) on first start
    /// and kept. Use it when the master can reach the node; use
    /// --master-url when only the other direction works, e.g. a node
    /// behind NAT.
    #[arg(long, env = "SANDSTORM_LISTEN", conflicts_with = "master_url")]
    listen: Option<SocketAddr>,
    #[arg(long, env = "SANDSTORM_NODE_ID")]
    node_id: Option<String>,
    /// Registration token. Not needed once this node holds a certificate,
    /// and not needed at all when --master runs in the same process.
    #[arg(long, env = "SANDSTORM_NODE_TOKEN")]
    node_token: Option<String>,

    #[arg(long, default_value = "/var/lib/sandstorm", env = "SANDSTORM_WORK_DIR")]
    work_dir: PathBuf,
    /// Which engine runs sandboxes: runsc (gVisor, the default) or
    /// firecracker (needs /dev/kvm, and a work dir on XFS or btrfs).
    #[arg(long, default_value = "runsc", value_parser = ["runsc", "firecracker"])]
    backend: String,
    /// A file naming the backend (`runsc` or `firecracker`), which wins over
    /// --backend once it exists. The installer writes one, so the master can
    /// switch the node's backend: it rewrites this file and the node
    /// restarts onto it (see `node/switch.rs`). Without it a node's backend
    /// is its command line's, and a switch from the console is refused.
    #[arg(long, env = "SANDSTORM_BACKEND_FILE")]
    backend_file: Option<PathBuf>,
    #[arg(long, default_value = "runsc")]
    runsc: String,
    #[arg(long, default_value = "firecracker")]
    firecracker: String,
    /// The guest kernel for --backend firecracker, an uncompressed vmlinux.
    #[arg(long, default_value = "/var/lib/sandstorm/vmlinux")]
    fc_kernel: PathBuf,
    #[arg(long, default_value_t = 1)]
    fc_vcpus: u8,
    /// Keep the guests' serial console on a PVM host, where it is off because
    /// it slows a boot by 150 ms or more. Their output goes to
    /// <work-dir>/logs/<id>.log. Other hosts always have it.
    #[arg(long)]
    fc_serial_console: bool,
    /// Network namespaces to keep built ahead of any sandbox, so a create or
    /// a wake from Snapshot only names and addresses one. 0 builds each on
    /// demand, as before. Each holds about 0.4 MiB of kernel memory. By
    /// default four for each GiB of memory, between 32 and 256: a burst past
    /// the pool builds its own at about 80 ms each, one at a time.
    #[arg(long, env = "SANDSTORM_NETNS_POOL")]
    netns_pool: Option<usize>,
    /// runsc's state directory. Per-deployment by default: sharing one
    /// means two sandstorm nodes would each see the other's containers as
    /// orphans and destroy them.
    #[arg(long, default_value = "/var/run/sandstorm-runsc")]
    runsc_root: PathBuf,
    /// Shared volume root. A JuiceFS mount in a fleet; local on one box.
    #[arg(long, env = "SANDSTORM_VOLUME_ROOT")]
    volume_root: Option<PathBuf>,
    /// 0 means "as many as memory allows".
    #[arg(long, default_value_t = 0)]
    max_sandboxes: u32,

    #[arg(long, default_value_t = 30)]
    idle_frozen_secs: u64,
    #[arg(long, default_value_t = 300)]
    idle_snapshot_secs: u64,
    #[arg(long, default_value_t = 3600)]
    idle_detached_secs: u64,
    /// How long the master parks a node's request when it has no work.
    /// Not a scheduling delay -- commands arrive on the parked request.
    #[arg(long, default_value_t = 25)]
    hold_secs: u64,

    /// Range sandbox ports are allocated from, per node.
    #[arg(long, default_value_t = 20000)]
    port_min: u16,
    #[arg(long, default_value_t = 32000)]
    port_max: u16,

    /// Bearer token the API requires. Read from this file; one is generated
    /// on first run if the file does not exist.
    ///
    /// Not optional in practice: code inside a sandbox can reach the master
    /// over loopback, so an open control plane lets a sandbox mint node
    /// tokens and delete other sandboxes. `--api-open` is the explicit
    /// opt-out for a throwaway single-node run.
    #[arg(long, env = "SANDSTORM_API_TOKEN_FILE")]
    api_token_file: Option<PathBuf>,
    /// Run the API with no authentication. Only for a node nothing else can
    /// reach, and never with untrusted sandboxes on it.
    #[arg(long)]
    api_open: bool,
    /// Serve the control plane as plain HTTP instead of TLS.
    ///
    /// TLS is the default because the control plane carries registration
    /// tokens, the API token and every command run inside a sandbox. Without
    /// it the master also cannot tell one node from another -- a bearer
    /// token is one shared secret, so any holder could poll as any node --
    /// so `--insecure-transport` turns the client-certificate check off too.
    #[arg(long)]
    insecure_transport: bool,

    /// Serve the E2B API and E2B's sandbox addresses on this address, for
    /// the official e2b SDKs. Needs --e2b-domain. Off when absent.
    #[arg(long, env = "SANDSTORM_E2B_BIND", requires = "e2b_domain")]
    e2b_bind: Option<std::net::SocketAddr>,
    /// What the SDKs' E2B_DOMAIN is set to. `api.<domain>` and
    /// `*.<domain>` must resolve to --e2b-bind.
    #[arg(long, env = "SANDSTORM_E2B_DOMAIN", requires = "e2b_bind")]
    e2b_domain: Option<String>,
    /// A certificate for `*.<domain>`, PEM. Without one the front door's
    /// certificate is issued from this deployment's CA (pki-dir/ca.pem),
    /// which clients then have to trust, e.g. with SSL_CERT_FILE.
    #[arg(long, requires = "e2b_tls_key")]
    e2b_tls_cert: Option<PathBuf>,
    #[arg(long, requires = "e2b_tls_cert")]
    e2b_tls_key: Option<PathBuf>,

    /// Range sandbox network namespaces are addressed from. Each sandbox
    /// gets a /30 out of it, so a /16 holds 16384 of them.
    #[arg(long, default_value = "10.86.0.0")]
    net_base: std::net::Ipv4Addr,
    #[arg(long, default_value_t = 16)]
    net_prefix: u8,

    /// Stop placing sandboxes on a node with less free memory than this.
    #[arg(long, default_value_t = 512)]
    admit_min_free_mib: u64,
    /// Stop placing sandboxes on a node whose 10-second memory-pressure
    /// average is above this. On bm, 10 was already degrading and 60 took
    /// the machine off the network.
    #[arg(long, default_value_t = 8.0)]
    admit_max_psi: f64,
    /// The same ceiling for waking a sandbox that already exists, when it
    /// should differ. Defaults to --admit-max-psi.
    #[arg(long)]
    wake_max_psi: Option<f64>,
    /// How long a wake may take to start -- waiting for a restore slot,
    /// then for memory pressure to ease -- before the request is answered
    /// 503 with retry-after. Overload is refused quickly rather than
    /// queued.
    #[arg(long, default_value_t = 2000)]
    wake_budget_ms: u64,
    /// How many wakes may wait for a restore slot at once; one further back
    /// is refused at once. Defaults to twice the slots (one per CPU).
    #[arg(long)]
    wake_max_waiting: Option<usize>,

    /// Squeeze a frozen sandbox's memory into compressed swap: auto, on or
    /// off. Auto turns it on where swap is zswap or zram, which on bm took
    /// a frozen 376 MiB heap to 113 MiB at the cost of a 310 ms thaw.
    #[arg(long, default_value = "auto")]
    reclaim_frozen: sandstorm_core::node::Lever,
    /// Compress snapshot images: auto, on or off. Auto measures the disk at
    /// startup and compresses where it reads slower than 500 MB/s, which is
    /// where a smaller image restores faster (on bm: 4.3x smaller, cold
    /// restore 3.3 s -> 1.4 s).
    #[arg(long, default_value = "auto")]
    compress_snapshots: sandstorm_core::node::Lever,
    /// Keep squeezing the cold pages of hot sandboxes into compressed swap,
    /// paced by each sandbox's own memory stalls: auto, on or off. Auto
    /// follows --reclaim-frozen's test: zswap or zram present.
    #[arg(long, default_value = "auto")]
    reclaim_hot: sandstorm_core::node::Lever,
    /// Read a snapshotted sandbox's image just before its next request when
    /// its requests come on a schedule: auto (= on), on or off.
    #[arg(long, default_value = "auto")]
    prefetch_snapshots: sandstorm_core::node::Lever,
    /// Give the reclaim levers compressed RAM to work with where there is
    /// none: turn zswap on in front of an existing disk swap, or add a zram
    /// device on a machine with no swap at all. auto or off. Host-wide; see
    /// `bootstrap_compressed_swap`.
    #[arg(long, default_value = "auto")]
    compressed_swap: sandstorm_core::node::Lever,
}

#[derive(Args)]
struct NodeTokenArgs {
    #[arg(long, default_value = "default")]
    pool: String,
    /// Short by design: a bootstrap credential should not outlive the
    /// terminal session that created it.
    #[arg(long, default_value_t = 900)]
    ttl_secs: i64,
    #[arg(
        long,
        default_value = "/var/lib/sandstorm/state.db",
        env = "SANDSTORM_STATE_DB"
    )]
    state_db: PathBuf,
}

#[derive(Args)]
struct StateArgs {
    #[arg(
        long,
        default_value = "/var/lib/sandstorm/state.db",
        env = "SANDSTORM_STATE_DB"
    )]
    state_db: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    match Cli::parse().cmd {
        Cmd::Serve(a) => serve(*a).await,
        Cmd::NodeToken(a) => {
            let store = Store::open(&a.state_db)?;
            let token = random_token();
            store.create_node_token(&token, &a.pool, a.ttl_secs)?;
            println!("{token}");
            eprintln!("pool={} valid for {}s, single use", a.pool, a.ttl_secs);
            Ok(())
        }
        Cmd::Nodes(a) => {
            let store = Store::open(&a.state_db)?;
            println!(
                "{:<20} {:<12} {:<18} {:<6} STATE",
                "ID", "POOL", "FINGERPRINT", "MAX"
            );
            for n in store.list_nodes()? {
                println!(
                    "{:<20} {:<12} {:<18} {:<6} {}",
                    n.id,
                    n.pool,
                    n.fingerprint,
                    n.max_sandboxes,
                    if n.connected { "connected" } else { "offline" }
                );
            }
            Ok(())
        }
        Cmd::Preflight(a) => {
            let kind: node::BackendKind = a.backend.parse()?;
            let cfg = RuntimeConfig {
                backend: kind,
                runsc: a.runsc,
                firecracker: a.firecracker,
                fc_kernel: a.fc_kernel,
                work_dir: a.work_dir,
                ..RuntimeConfig::default()
            };
            let engine = node::preflight(&cfg, kind)
                .with_context(|| format!("this machine cannot run {kind}"))?;
            println!("{kind} ok: {engine}");
            Ok(())
        }
    }
}

/// Where each master-and-node on this machine records the gateway ports it
/// allocates from, one file per process.
const PORT_CLAIMS: &str = "/run/sandstorm/gateway-ports";

/// Refuse to start on a port range another `sandstormd` on this machine is
/// already allocating gateway ports from.
///
/// Every master defaults to 20000-32000 and allocates without knowing of any
/// other, while the gateways bind on the node's host. Two master-and-node
/// processes on one machine therefore hand out the same ports, and the second
/// sandbox on one fails to bind and is never reachable: on bare metal a
/// gVisor and a Firecracker node both gave out 20000, and the second
/// sandbox's gateway logged `Address already in use` every 25 s while its
/// caller waited. Only checked when both halves run here, the case where
/// the range and the binds are on the same machine.
fn claim_port_range(min: u16, max: u16, work_dir: &std::path::Path) -> Result<()> {
    let dir = std::path::Path::new(PORT_CLAIMS);
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let me = std::process::id();
    for e in std::fs::read_dir(dir)?.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if pid == me || !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            let _ = std::fs::remove_file(e.path());
            continue;
        }
        let claim = std::fs::read_to_string(e.path()).unwrap_or_default();
        let mut f = claim.split_whitespace();
        let (Some(lo), Some(hi)) = (
            f.next().and_then(|v| v.parse::<u16>().ok()),
            f.next().and_then(|v| v.parse::<u16>().ok()),
        ) else {
            continue;
        };
        if lo <= max && min <= hi {
            let theirs = f.next().unwrap_or("?");
            bail!(
                "gateway ports {min}-{max} overlap {lo}-{hi}, which sandstormd pid {pid} \
                 (work dir {theirs}) allocates from on this machine; give each its own \
                 range with --port-min and --port-max"
            );
        }
    }
    std::fs::write(
        dir.join(me.to_string()),
        format!("{min} {max} {}\n", work_dir.display()),
    )
    .with_context(|| format!("recording this node's ports in {}", dir.display()))?;
    Ok(())
}

async fn serve(a: ServeArgs) -> Result<()> {
    if !a.master && !a.node {
        bail!("pass --master, --node, or both");
    }
    raise_open_file_limit();
    if a.master && a.node {
        claim_port_range(a.port_min, a.port_max, &a.work_dir)?;
    }

    let idle = IdlePolicy {
        to_frozen: Duration::from_secs(a.idle_frozen_secs),
        to_snapshot: Duration::from_secs(a.idle_snapshot_secs),
        to_detached: Duration::from_secs(a.idle_detached_secs),
    };

    // Minted before either half starts, so the node in this same process
    // can read it too.
    let api_token = if a.api_open {
        tracing::warn!(
            "--api-open: the API requires no token. A sandbox on this node can \
             reach it and mint node tokens or delete sandboxes."
        );
        None
    } else {
        let path = a
            .api_token_file
            .clone()
            .unwrap_or_else(|| a.pki_dir.join("api-token"));
        if a.master {
            // The master owns this token: it is the credential its API
            // checks, so if there is no file it makes one.
            Some(load_or_create_token(&path)?)
        } else {
            // A node must NOT mint one. The token it needs is the
            // master's, learned at registration -- and `node::serve`
            // only adopts the learned one when it does not already have
            // a token, so a node that invented its own would keep it and
            // poll with a value the master has never seen.
            //
            // That is a 401 on every poll, and it cannot happen on one
            // machine: a combined `--master --node` shares this pki dir,
            // so the node reads the master's own token and matches. It
            // takes two machines to see it, which is where it was found.
            let stored = std::fs::read_to_string(&path)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            if stored.is_none() && a.api_token_file.is_some() {
                anyhow::bail!(
                    "--api-token-file {} is empty or missing; a node does not \
                     create one, it is given the master's at registration",
                    path.display()
                );
            }
            stored
        }
    };

    let master_fut = if a.master {
        let cfg = MasterConfig {
            bind: a.bind,
            tls_sans: a.tls_san.clone(),
            state_db: a.state_db.clone(),
            pki_dir: a.pki_dir.clone(),
            idle,
            api_token: api_token.clone(),
            // A /30 each, minus the one slot that would run past the end.
            // One short of the plan: the top slot is the node's own, for the
            // seed a warm template is taken from (`NetConfig::reserved_index`).
            max_net_index: (1u32 << (32 - a.net_prefix.min(30) as u32)) / 4 - 2,
            port_range: (a.port_min, a.port_max),
            insecure_transport: a.insecure_transport,
            e2b: match (a.e2b_bind, a.e2b_domain.clone()) {
                (Some(bind), Some(domain)) => Some(master::FrontConfig {
                    bind,
                    domain,
                    tls_cert: a.e2b_tls_cert.clone(),
                    tls_key: a.e2b_tls_key.clone(),
                }),
                _ => None,
            },
            admit: sandstorm_core::master::Admission {
                min_free_mib: a.admit_min_free_mib,
                max_psi_avg10: a.admit_max_psi,
            },
        };
        Some(tokio::spawn(master::serve(cfg)))
    } else {
        None
    };

    let node_fut = if a.node {
        // Listening, there is no master to dial and nothing to register
        // with: the master comes to us with the pairing code.
        let listening = a.listen;
        let master_url = match a.master_url.clone() {
            _ if listening.is_some() => String::new(),
            Some(u) => u,
            None if a.master => {
                let scheme = if a.insecure_transport {
                    "http"
                } else {
                    "https"
                };
                format!("{scheme}://{}", a.bind)
            }
            None => bail!("--node without --master needs --master-url"),
        };
        let node_id = a
            .node_id
            .clone()
            .or_else(hostname)
            .unwrap_or_else(|| format!("node-{}", &random_token()[..8]));

        // In-process master means we can mint our own registration token
        // rather than making the operator copy one between two terminals.
        let token = match a.node_token.clone() {
            _ if listening.is_some() => None,
            Some(t) => Some(t),
            None if a.master => {
                // Give the master a moment to create the DB and the CA.
                tokio::time::sleep(Duration::from_millis(300)).await;
                let store = Store::open(&a.state_db)?;
                let secret = random_token();
                store.create_node_token(&secret, "default", 300)?;
                // Minted with the CA fingerprint, exactly as
                // `/v1/node-tokens` does. Going straight to the store
                // skipped that, so the node had nothing to verify the
                // master with and its own loopback handshake failed with
                // `invalid peer certificate: UnknownIssuer` -- which broke
                // `serve --master --node`, the single-command deployment,
                // the moment TLS became the default. Both earlier mTLS
                // checks ran the two halves as separate processes with
                // tokens minted through the API, so neither saw it.
                let secret = match std::fs::read_to_string(a.pki_dir.join("ca.pem"))
                    .ok()
                    .and_then(|pem| sandstorm_core::tls::fingerprint_of_pem(&pem).ok())
                {
                    Some(pin) => sandstorm_core::tls::join_token(&pin, &secret),
                    None if a.insecure_transport => secret,
                    None => bail!(
                        "the master has not written {} yet, so this node cannot \
                         pin it; retry, or pass --insecure-transport for a \
                         plain-HTTP single-box run",
                        a.pki_dir.join("ca.pem").display()
                    ),
                };
                Some(secret)
            }
            None => None,
        };

        // Probed once, here, so the reasons land in the startup log where an
        // operator looks for them.
        // First, so the reclaim decisions below see the swap it makes.
        let (made, why) = node::bootstrap_compressed_swap(a.compressed_swap);
        tracing::info!(changed = made, reason = %why, "compressed swap");
        let (reclaim_frozen, why) = node::decide_reclaim(a.reclaim_frozen);
        tracing::info!(on = reclaim_frozen, reason = %why, "frozen-sandbox reclaim");
        let (reclaim_hot, why) = node::decide_reclaim(a.reclaim_hot);
        tracing::info!(on = reclaim_hot, reason = %why, "hot-sandbox reclaim");
        let (compress_snapshots, why) = node::decide_compression(a.compress_snapshots, &a.work_dir);
        tracing::info!(on = compress_snapshots, reason = %why, "snapshot compression");

        // The backend file, when there is one, is the node's own record of
        // the operator's choice and wins over the flag.
        let backend = match a.backend_file.as_deref().map(node::read_backend_file) {
            Some(Ok(Some(kind))) => kind,
            Some(Err(e)) => return Err(e),
            _ => a.backend.parse()?,
        };
        let mut features = Vec::new();
        if a.backend_file.is_some() {
            features.push("backend-switch".to_string());
        }
        if let Some(kind) = node::compressed_swap() {
            features.push(format!("swap:{kind}"));
        }
        for (on, name) in [
            (reclaim_frozen, "reclaim-frozen"),
            (reclaim_hot, "reclaim-hot"),
            (compress_snapshots, "snapshot-compression"),
            (
                a.prefetch_snapshots != node::Lever::Off,
                "prefetch-snapshots",
            ),
        ] {
            if on {
                features.push(name.to_string());
            }
        }

        let cfg = NodeConfig {
            node_id,
            master_url,
            api_token: api_token.clone(),
            token,
            pki_dir: a.pki_dir.join("node"),
            runtime: RuntimeConfig {
                backend,
                runsc: a.runsc.clone(),
                runsc_root: a.runsc_root.clone(),
                firecracker: a.firecracker.clone(),
                fc_kernel: a.fc_kernel.clone(),
                fc_vcpus: a.fc_vcpus,
                fc_serial_console: a.fc_serial_console,
                work_dir: a.work_dir.clone(),
                max_sandboxes: a.max_sandboxes,
                volume_root: a
                    .volume_root
                    .clone()
                    .unwrap_or_else(|| a.work_dir.join("volumes")),
                net: sandstorm_core::node::NetConfig {
                    base: a.net_base,
                    prefix: a.net_prefix,
                },
                reclaim_frozen,
                compress_snapshots,
                reclaim_hot,
                prefetch_snapshots: a.prefetch_snapshots != node::Lever::Off,
                pin_in_zswap: node::pin_in_zswap(),
                reclaim_floor_mib: a.admit_min_free_mib,
                netns_pool: a
                    .netns_pool
                    .unwrap_or_else(sandstorm_core::node::default_netns_pool),
            },
            hold: Duration::from_secs(a.hold_secs),
            // The same two numbers the master places with, applied here
            // against a reading taken now rather than one up to a hold old.
            admit_min_free_mib: a.admit_min_free_mib,
            admit_max_psi: a.admit_max_psi,
            wake_max_psi: a.wake_max_psi.unwrap_or(a.admit_max_psi),
            wake_budget: Duration::from_millis(a.wake_budget_ms),
            wake_max_waiting: a.wake_max_waiting,
            features,
            fetches: Default::default(),
            pulls: Default::default(),
            backend_file: a.backend_file.clone(),
        };
        Some(match listening {
            Some(addr) => tokio::spawn(node::serve_listen(cfg, addr)),
            None => tokio::spawn(node::serve(cfg)),
        })
    } else {
        None
    };

    // Either half failing takes the process down; systemd restarts it, and
    // the sandboxes do not care because they live in their own scopes.
    match (master_fut, node_fut) {
        (Some(m), Some(n)) => tokio::select! {
            r = m => r??,
            r = n => r??,
        },
        (Some(m), None) => m.await??,
        (None, Some(n)) => n.await??,
        (None, None) => unreachable!("checked above"),
    }
    Ok(())
}

/// Read the API token, creating it on first run.
///
/// Generated rather than required so a first run is still one command, and
/// written at 0600 with its path logged so the operator can find it.
/// Raise the soft open-file limit to the hard one.
///
/// A node holds descriptors for every sandbox it has, asleep or not: the
/// gateway keeps a listener open for frozen and snapshotted sandboxes too, so
/// that a request can wake them. systemd starts services at a soft limit of
/// 1024 whatever the hard limit is, and on the 2026-10-26 fleet run every
/// node stopped at about a thousand sandboxes with `ip netns add` failing
/// "No file descriptors available" -- 5,876 across six machines that had
/// the memory for more. Nothing about that error says "limit".
///
/// The hard limit is the operator's ceiling and stays theirs; this only
/// stops leaving most of it unused.
fn raise_open_file_limit() {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit and setrlimit only read and write the struct passed.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 || lim.rlim_cur >= lim.rlim_max {
            return;
        }
        let from = lim.rlim_cur;
        lim.rlim_cur = lim.rlim_max;
        if libc::setrlimit(libc::RLIMIT_NOFILE, &lim) == 0 {
            tracing::info!(from, to = lim.rlim_max, "raised the open-file limit");
        } else {
            tracing::warn!(
                limit = from,
                "could not raise the open-file limit; a node stops at about this many sandboxes"
            );
        }
    }
}

fn load_or_create_token(path: &std::path::Path) -> Result<String> {
    if let Ok(t) = std::fs::read_to_string(path) {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Ok(t);
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let token = random_token();
    std::fs::write(path, &token)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    tracing::info!(path = %path.display(), "generated an API token");
    Ok(token)
}

fn hostname() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
