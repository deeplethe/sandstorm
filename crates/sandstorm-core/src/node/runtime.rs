//! Tier changes, executed against the node's backend (`backend.rs`).
//!
//! Every measurement, made with the runsc backend, in the tier table came from this sequence on bm:
//!   hot -> frozen    `echo 1 > cgroup.freeze`          8-11 ms, 0 CPU
//!   frozen -> hot    `echo 0 > cgroup.freeze`          8-11 ms
//!   hot -> snapshot  `runsc checkpoint` + delete       84 ms, frees RAM
//!   snapshot -> hot  `runsc restore`                   84 ms
//!   hot -> detached  tar+zstd the layer into the volume
//!   detached -> hot  untar + cold start                1.7-2.8 s
//!
//! A started sandbox is not a child of this process, so a control-plane
//! restart does not touch it. The cgroup is named in the bundle's spec, so there is exactly one path
//! to write for a freeze.

use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command as Exec, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::backend::{Backend, Runsc};
use super::bundle::{self, TemplateSpec, WarmSpec};
use super::firecracker::{self, Firecracker};
use super::net::{self, NetConfig};
use crate::proto::{HostInfo, ObservedSandbox};
use crate::tier::Tier;

/// What one sandbox is created from.
pub struct CreateRequest<'a> {
    pub id: &'a str,
    pub template: &'a str,
    pub spec: &'a serde_json::Value,
    pub port: u16,
    pub volume: Option<&'a str>,
    pub net_index: Option<u32>,
    /// Fork from this tag's checkpoint rather than starting cold.
    pub from_tag: Option<&'a str>,
    /// Host port the gateway should listen on, and the token it checks.
    /// Both are written into the bundle so a node restart re-adopts the
    /// sandbox with its gateway intact -- otherwise the port would come
    /// back open with nothing to verify.
    pub gateway_port: Option<u16>,
    pub token: &'a str,
    pub egress: &'a crate::proto::Egress,
}

/// The node's own seed sandboxes, which a warm template image is taken
/// from. `observe` never reports them, and the master refuses the prefix
/// for anything it creates, so the two can never be confused.
pub const WARM_SEED_PREFIX: &str = "sandstorm-warm-";
/// How long a failed seed keeps a template cold before it is tried again.
const WARM_RETRY: Duration = Duration::from_secs(600);
/// How long a sandbox has to stay up after a restore before the image it came
/// from may go; see `Runtime::may_restore`.
pub const RESTORE_SURVIVAL: Duration = Duration::from_secs(60);
/// In a bundle: when the sandbox was last restored from an image it depends
/// on. On disk, so a node restarted in between still knows.
const RESTORED: &str = "restored";

/// A sandbox started from a warm image and gone again within this is taken
/// to have died of the image.
const WARM_SURVIVAL: Duration = Duration::from_secs(30);
/// How long `after_restore` may take.
const AFTER_RESTORE_TIMEOUT: Duration = Duration::from_secs(10);

/// Which engine runs this node's sandboxes. `backend.rs`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BackendKind {
    /// gVisor. Needs no `/dev/kvm`.
    #[default]
    Runsc,
    /// Firecracker microVMs, for hosts with `/dev/kvm`.
    Firecracker,
}

impl BackendKind {
    /// The name `--backend` and a node's backend file use.
    pub fn as_str(self) -> &'static str {
        match self {
            BackendKind::Runsc => "runsc",
            BackendKind::Firecracker => "firecracker",
        }
    }
}

impl std::fmt::Display for BackendKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for BackendKind {
    type Err = anyhow::Error;
    /// `gvisor` is accepted for runsc, since that is what the console and
    /// the installer call it.
    fn from_str(s: &str) -> Result<Self> {
        match s.trim() {
            "runsc" | "gvisor" => Ok(BackendKind::Runsc),
            "firecracker" => Ok(BackendKind::Firecracker),
            other => bail!("unknown backend {other:?}: runsc (gvisor) or firecracker"),
        }
    }
}

/// Whether this host can run `kind` as `cfg` would configure it, and the
/// engine's `--version` if it can.
///
/// The same checks the node makes when it starts on that backend, so the
/// installer (`sandstormd preflight`) and a backend switch refuse exactly
/// where the node itself would. Firecracker: `/dev/kvm`, the guest kernel,
/// a work dir that can reflink, and a binary that answers. runsc: a binary
/// that answers.
pub fn preflight(cfg: &RuntimeConfig, kind: BackendKind) -> Result<String> {
    let engine: Box<dyn Backend> = match kind {
        BackendKind::Runsc => Box::new(Runsc {
            bin: cfg.runsc.clone(),
            root: cfg.runsc_root.clone(),
            work_dir: cfg.work_dir.clone(),
        }),
        BackendKind::Firecracker => {
            let fc = Firecracker {
                bin: cfg.firecracker.clone(),
                kernel: cfg.fc_kernel.clone(),
                vcpus: cfg.fc_vcpus,
                work_dir: cfg.work_dir.clone(),
                serial_console: cfg.fc_serial_console || !firecracker::pvm_host(),
            };
            std::fs::create_dir_all(&cfg.work_dir)
                .with_context(|| format!("creating {}", cfg.work_dir.display()))?;
            fc.check_host()?;
            Box::new(fc)
        }
    };
    let version = engine.version();
    if version.trim().is_empty() {
        let bin = match kind {
            BackendKind::Runsc => &cfg.runsc,
            BackendKind::Firecracker => &cfg.firecracker,
        };
        bail!("{bin} --version did not answer; is the {kind} binary installed?");
    }
    Ok(version)
}

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub backend: BackendKind,
    pub runsc: String,
    pub runsc_root: PathBuf,
    pub firecracker: String,
    /// The guest kernel, an uncompressed vmlinux.
    pub fc_kernel: PathBuf,
    pub fc_vcpus: u8,
    /// A serial console in guests on a PVM host too; see `pvm_host`.
    pub fc_serial_console: bool,
    /// Bundles, writable layers and snapshot images live here.
    pub work_dir: PathBuf,
    pub max_sandboxes: u32,
    /// Where detached sandboxes keep their archive. A shared volume
    /// (JuiceFS) if there is one, otherwise local.
    pub volume_root: PathBuf,
    /// Address plan for per-sandbox network namespaces.
    pub net: NetConfig,
    /// Push a frozen sandbox's memory into compressed swap. Only worth it
    /// where swap is zswap or zram; see `host.rs`, which decides.
    pub reclaim_frozen: bool,
    /// Compress the images Snapshot writes. Worth it on a slow disk; see
    /// `host.rs`, which decides.
    pub compress_snapshots: bool,
    /// Keep squeezing the cold pages of hot sandboxes into compressed swap,
    /// paced by their own memory stalls; see `reclaim.rs`.
    pub reclaim_hot: bool,
    /// Read a periodic sandbox's snapshot image just before its next
    /// predicted request; see `wake.rs`.
    pub prefetch_snapshots: bool,
    /// Turn zswap writeback off for reclaimed sandboxes. Right when swap is
    /// zswap over a disk, wrong over zram; `host::pin_in_zswap` decides.
    pub pin_in_zswap: bool,
    /// Reclaim, frozen or hot, stops while swap is under the floor a node
    /// with this memory floor keeps (`HostReading::swap_floor_mib`): every
    /// page it squeezes out takes a swap slot. The admission floor; 0
    /// squeezes whatever is left.
    pub reclaim_floor_mib: u64,
    /// Network namespaces kept built ahead of any sandbox; see `net::Pool`.
    /// 0 builds each one when it is needed.
    pub netns_pool: usize,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            backend: BackendKind::Runsc,
            runsc: "runsc".into(),
            runsc_root: "/var/run/sandstorm-runsc".into(),
            firecracker: "firecracker".into(),
            fc_kernel: "/var/lib/sandstorm/vmlinux".into(),
            fc_serial_console: false,
            fc_vcpus: 1,
            work_dir: "/var/lib/sandstorm".into(),
            max_sandboxes: 0,
            volume_root: "/var/lib/sandstorm/volumes".into(),
            net: NetConfig::default(),
            reclaim_frozen: false,
            compress_snapshots: false,
            reclaim_hot: false,
            prefetch_snapshots: false,
            pin_in_zswap: true,
            reclaim_floor_mib: 0,
            netns_pool: 0,
        }
    }
}

/// Reclaim a frozen sandbox's memory, off the command path.
///
/// One worker for the whole node, taking sandboxes in the order they were
/// frozen: reclaim costs CPU to compress (1.3 s for 376 MiB on bm), and a
/// wave of idle demotions should not turn into a wave of threads. In 64 MiB
/// steps, so a sandbox thawed partway through -- woken by a request -- stops
/// being squeezed at once.
fn reclaim_later(id: &str, cg: PathBuf, pin: bool, floor_mib: u64) {
    use std::sync::mpsc::{channel, Sender};
    use std::sync::{Mutex, OnceLock};
    type Job = (String, PathBuf, bool, u64);
    static QUEUE: OnceLock<Mutex<Sender<Job>>> = OnceLock::new();
    let q = QUEUE.get_or_init(|| {
        let (tx, rx) = channel::<Job>();
        std::thread::Builder::new()
            .name("sandstorm-reclaim".into())
            .spawn(move || {
                for (id, cg, pin, floor_mib) in rx {
                    reclaim_frozen(&id, &cg, pin, floor_mib);
                }
            })
            .expect("spawning the reclaim worker");
        Mutex::new(tx)
    });
    if let Ok(tx) = q.lock() {
        let _ = tx.send((id.to_string(), cg, pin, floor_mib));
    }
}

/// Where a sandbox's reclaimed pages go: kept in zswap's pool when the swap
/// behind it is a disk, or past zswap straight to the device when that
/// device is zram -- otherwise every page is compressed twice, once into
/// zswap and again when zswap hands it on.
pub(super) fn route_reclaim(cg: &Path, pin_in_zswap: bool) {
    if pin_in_zswap {
        let _ = std::fs::write(cg.join("memory.zswap.writeback"), "0");
    } else {
        let _ = std::fs::write(cg.join("memory.zswap.max"), "0");
    }
}

fn reclaim_frozen(id: &str, cg: &Path, pin: bool, floor_mib: u64) {
    let frozen = || {
        std::fs::read_to_string(cg.join("cgroup.freeze"))
            .map(|s| s.trim() == "1")
            .unwrap_or(false)
    };
    let mib = || {
        std::fs::read_to_string(cg.join("memory.current"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map_or(0, |b| b >> 20)
    };
    if !frozen() {
        return;
    }
    // Keep what is reclaimed in compressed RAM rather than letting zswap
    // write it on to disk: that is the difference between 310 ms and 3.9 s
    // to page it all back. Kernels before 6.8 have no such knob, and there
    // some pages may reach disk -- slower to wake, never wrong.
    route_reclaim(cg, pin);
    let (t0, before) = (Instant::now(), mib());
    // The kernel answers EAGAIN when it cannot take the WHOLE amount asked
    // for, having taken what it could. Stopping at the first one left 135
    // of 185 MiB on bm; halving the ask and carrying on gets the rest.
    let mut ask = 64u64;
    for _ in 0..(before / 4 + 16) {
        if !frozen() {
            break;
        }
        if host_reading().swap_short(floor_mib) {
            tracing::debug!(%id, "swap under its floor; frozen reclaim stops");
            break;
        }
        if std::fs::write(cg.join("memory.reclaim"), format!("{ask}M")).is_err() {
            if ask == 4 {
                break;
            }
            ask /= 2;
        }
    }
    tracing::info!(
        %id, before_mib = before, after_mib = mib(), ms = t0.elapsed().as_millis() as u64,
        "reclaimed a frozen sandbox"
    );
}

/// When the host directories a template binds in last had a file added or
/// replaced, as far as one level down tells, in nanoseconds since the epoch.
///
/// A warm image holds a running program's memory, laid out for the files it
/// had mapped. On gVisor those are the host's, bound in, so an upgrade under
/// an image leaves it pointing into files that have moved. Measured on bare
/// metal: unattended-upgrades replaced python3.12 and libc 18 hours after an
/// image was taken, and every sandbox restored from it segfaulted 200 ms
/// later. A package manager replaces a file by renaming over it, which is
/// what a directory's mtime records, and this is on the path of every create,
/// so it stops at a level that catches `/usr/bin` and the library directories
/// without walking the tree. `drop_warm_image` is there for what it misses.
fn binds_changed_at(binds: &[String]) -> u128 {
    let at = |p: &Path| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos())
    };
    binds
        .iter()
        .map(|bind| {
            let bind = Path::new(bind);
            let below = std::fs::read_dir(bind)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| at(&e.path()))
                .max();
            at(bind).max(below.unwrap_or(0))
        })
        .max()
        .unwrap_or(0)
}

/// A warm image that a sandbox did not outlive: delete it, so the next create
/// seeds another, unless `ready` says it has already been replaced.
///
/// A create that finds its sandbox dead just after a warm start is the only
/// sign left of an image gone wrong in a way the key missed. Without this
/// every create on the node starts twice, for good. An image this young did
/// not go stale, so it is the program that does not survive being restored,
/// and seeding again at once would only repeat it.
fn drop_warm_image(id: &str, dir: &Path, ready: Option<std::time::SystemTime>) {
    let now = std::fs::metadata(dir.join("ready"))
        .and_then(|m| m.modified())
        .ok();
    if now.is_none() || now != ready {
        return;
    }
    tracing::warn!(%id, image = %dir.display(), "a sandbox died just after a warm start; dropping the image");
    let young = now
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < WARM_RETRY);
    if young {
        let _ = std::fs::write(
            dir.with_extension("failed"),
            "its sandboxes did not survive\n",
        );
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// Holds no state of its own beyond its configuration and its backend --
/// everything it knows it reads from disk and the cgroup tree -- so a clone
/// is a second handle on the same node, which is what the gateway's waker
/// uses.
#[derive(Clone)]
pub struct Runtime {
    cfg: RuntimeConfig,
    engine: Arc<dyn Backend>,
    /// Asking the backend runs a subprocess; the answer cannot change
    /// while this node runs.
    fp: Arc<std::sync::OnceLock<(String, HostInfo)>>,
    /// Namespaces built ahead of time, when `netns_pool` asks for any.
    pool: Option<Arc<net::Pool>>,
    /// Sandboxes still filling their memory; see `starting.rs`.
    starting: Arc<super::starting::Starting>,
    egress: Arc<net::EgressRules>,
    /// Sandboxes just started from a warm image; see `drop_warm_image`.
    warm_started: Arc<parking_lot::Mutex<HashMap<String, WarmStart>>>,
}

/// Which image a sandbox was started from, and when.
struct WarmStart {
    dir: PathBuf,
    ready: Option<std::time::SystemTime>,
    at: Instant,
}

impl Runtime {
    /// A node on the backend its configuration names.
    pub fn new(cfg: RuntimeConfig) -> Result<Self> {
        let engine: Arc<dyn Backend> = match cfg.backend {
            BackendKind::Runsc => Arc::new(Runsc {
                bin: cfg.runsc.clone(),
                root: cfg.runsc_root.clone(),
                work_dir: cfg.work_dir.clone(),
            }),
            BackendKind::Firecracker => {
                let fc = Firecracker {
                    bin: cfg.firecracker.clone(),
                    // Absolute: Firecracker runs in each sandbox's own disk
                    // directory, where a relative path would mean nothing.
                    kernel: std::fs::canonicalize(&cfg.fc_kernel)
                        .unwrap_or_else(|_| cfg.fc_kernel.clone()),
                    vcpus: cfg.fc_vcpus,
                    work_dir: cfg.work_dir.clone(),
                    serial_console: cfg.fc_serial_console || !firecracker::pvm_host(),
                };
                std::fs::create_dir_all(&cfg.work_dir)?;
                fc.check_host()?;
                Arc::new(fc)
            }
        };
        Self::with_backend(cfg, engine)
    }

    pub fn with_backend(cfg: RuntimeConfig, engine: Arc<dyn Backend>) -> Result<Self> {
        for d in [&cfg.work_dir, &cfg.volume_root] {
            std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
        }
        let pool = (cfg.netns_pool > 0).then(|| {
            let e = engine.clone();
            net::Pool::start(
                cfg.netns_pool,
                &cfg.work_dir.to_string_lossy(),
                Box::new(move |ns| e.prepare_netns(ns)),
            )
        });
        Ok(Self {
            cfg,
            engine,
            fp: Arc::default(),
            pool,
            starting: Arc::default(),
            egress: Arc::default(),
            warm_started: Arc::default(),
        })
    }

    /// What this node is snapshot-compatible with; see `host`.
    pub fn fingerprint(&self) -> String {
        self.fp
            .get_or_init(|| host(&self.engine.version()))
            .0
            .clone()
    }

    /// The backend this node runs, and how it was configured.
    pub fn config(&self) -> &RuntimeConfig {
        &self.cfg
    }

    /// Templates installed in the form this node's backend reads, sorted.
    /// A template installed only for the other backend is not here.
    pub fn templates(&self) -> Vec<String> {
        let dir = self.cfg.work_dir.join("templates");
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| self.engine.template_root(&e.path()).exists())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with('.'))
            .collect();
        names.sort();
        names
    }

    /// Every sandbox with a bundle here, the node's own warm seeds included:
    /// what a backend switch would strand. `observe` leaves the seeds out.
    pub fn bundles(&self) -> Vec<String> {
        let mut ids: Vec<String> = std::fs::read_dir(self.cfg.work_dir.join("bundles"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        ids.sort();
        ids
    }

    /// What `fingerprint` is made of; see `host`.
    pub fn host(&self) -> HostInfo {
        self.fp
            .get_or_init(|| host(&self.engine.version()))
            .1
            .clone()
    }

    /// What this node actually holds right now, read from the filesystem
    /// and cgroups. No subprocess, and in particular no `runsc list`.
    ///
    /// `runsc list` queries each sandbox's sentry over its socket, and a
    /// frozen sentry cannot answer: measured on bm, `runsc list` times out
    /// while any sandbox is frozen and succeeds the instant it is thawed.
    /// Since freezing is the whole point of the frozen tier, asking runsc
    /// would mean one frozen sandbox blinds the node to every other -- the
    /// node stops polling, and nothing recovers until someone thaws it by
    /// hand. So state comes from places that cannot block:
    ///
    ///   cgroup exists and has processes  -> Frozen or Hot, per cgroup.freeze
    ///   a whole image at snapshots/<id>  -> Snapshot (see `has_snapshot`)
    ///   neither                          -> not here; the master will Create
    ///
    /// Enumerating our own bundles directory also means we can only ever
    /// see our own sandboxes, which is stronger than filtering someone
    /// else's output: anything we report that desired state does not list
    /// gets destroyed.
    pub fn observe(&self) -> Result<Vec<ObservedSandbox>> {
        let mut found = Vec::new();
        let bundles = self.cfg.work_dir.join("bundles");
        let rd = match std::fs::read_dir(&bundles) {
            Ok(rd) => rd,
            // Nothing created yet: not an error, just an empty node.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(found),
            Err(e) => return Err(e).context("listing bundles"),
        };

        for entry in rd.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            if id.starts_with(WARM_SEED_PREFIX) {
                continue;
            }

            // The cgroup first: a sandbox with processes is running whatever
            // is on disk, such as an image a restore did not get to delete.
            let cg = self.cgroup_path(&id);
            let running = std::fs::read_to_string(cg.join("cgroup.procs"))
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
            if !running && self.has_snapshot(&id) {
                found.push(ObservedSandbox {
                    ip: self.ip_of(&id),
                    net_index: self.net_index_of(&id),
                    egress: Some(self.egress.of(&id)),
                    id,
                    tier: Tier::Snapshot,
                    serving: false,
                });
                continue;
            }
            if !running {
                // The bundle is here but nothing is in its cgroup: a failed
                // start, or a sandbox that died. Reporting it as present
                // would stop the master retrying it.
                continue;
            }
            let frozen = self.is_frozen(&id);
            found.push(ObservedSandbox {
                ip: self.ip_of(&id),
                net_index: self.net_index_of(&id),
                egress: Some(self.egress.of(&id)),
                id,
                tier: if frozen { Tier::Frozen } else { Tier::Hot },
                serving: !frozen,
            });
        }
        Ok(found)
    }

    /// Sandboxes that have died: a bundle, nothing in the cgroup, no
    /// snapshot, for longer than `min_age`.
    ///
    /// `observe` leaves these out -- reporting one as present would stop the
    /// master retrying a failed start -- so the master never asks for them to
    /// be destroyed either, and their network outlives them. On f9 one killed
    /// by the OOM killer kept its veth while the master handed its slot to a
    /// new sandbox, whose address then routed to the dead one.
    pub fn dead(&self, min_age: Duration) -> Vec<String> {
        let Ok(rd) = std::fs::read_dir(self.cfg.work_dir.join("bundles")) else {
            return Vec::new();
        };
        rd.flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
            .filter(|(id, _)| !id.starts_with(WARM_SEED_PREFIX))
            .filter(|(id, _)| !self.has_snapshot(id))
            .filter(|(id, _)| {
                std::fs::read_to_string(self.cgroup_path(id).join("cgroup.procs"))
                    .map(|s| s.trim().is_empty())
                    .unwrap_or(true)
            })
            .filter(|(_, path)| {
                std::fs::metadata(path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age >= min_age)
            })
            .map(|(id, _)| id)
            .collect()
    }

    fn cgroup_path(&self, id: &str) -> PathBuf {
        PathBuf::from("/sys/fs/cgroup/sandstorm").join(id)
    }

    /// Read a snapshotted sandbox's image so it is in the page cache when a
    /// request wakes it. Returns the bytes read, or `None` when there is no
    /// image -- the sandbox is not snapshotted, or has gone.
    pub fn prefetch_snapshot(&self, id: &str) -> Result<Option<u64>> {
        use std::io::Read;
        let dir = self.cfg.work_dir.join("snapshots").join(id);
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return Ok(None);
        };
        let mut buf = vec![0u8; 1 << 20];
        let mut total = 0u64;
        for e in rd.flatten() {
            if !e.path().is_file() {
                continue;
            }
            let mut f = std::fs::File::open(e.path())?;
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                total += n as u64;
            }
        }
        Ok(Some(total))
    }

    pub(super) fn pins_in_zswap(&self) -> bool {
        self.cfg.pin_in_zswap
    }

    pub(super) fn reclaim_floor_mib(&self) -> u64 {
        self.cfg.reclaim_floor_mib
    }

    /// The sandbox's cgroup, for the node's other workers.
    pub(super) fn cgroup_dir(&self, id: &str) -> PathBuf {
        self.cgroup_path(id)
    }

    fn is_frozen(&self, id: &str) -> bool {
        std::fs::read_to_string(self.cgroup_path(id).join("cgroup.freeze"))
            .map(|s| s.trim() == "1")
            .unwrap_or(false)
    }

    /// Memory the sandbox is actually holding, for the density accounting.
    /// `id` has started, and may take up to `limit_mib` as its program
    /// gets going.
    pub fn note_started(&self, id: &str, limit_mib: Option<u64>) {
        self.starting.add(id, limit_mib, std::time::Instant::now());
    }

    /// `id` has been let through and is about to start.
    pub fn note_starting(&self, id: &str, limit_mib: Option<u64>) {
        self.starting
            .promise(id, limit_mib, std::time::Instant::now());
    }

    /// `id` was let through and will not start after all.
    pub fn note_not_starting(&self, id: &str) {
        self.starting.forget(id);
    }

    /// How much of the free memory is spoken for by sandboxes still filling
    /// theirs.
    pub fn starting_owed_mib(&self) -> u64 {
        self.starting
            .owed_mib(std::time::Instant::now(), |id| self.memory_mib(id))
    }

    pub fn memory_mib(&self, id: &str) -> Option<u64> {
        std::fs::read_to_string(self.cgroup_path(id).join("memory.current"))
            .ok()?
            .trim()
            .parse::<u64>()
            .ok()
            .map(|b| b / 1048576)
    }

    /// Bring a sandbox up from nothing.
    ///
    /// The template is resolved on the node, not passed down from the
    /// master: the rootfs is a local path, and a master should not be able
    /// to name an arbitrary directory on a node's disk.
    pub fn create(&self, req: &CreateRequest<'_>) -> Result<()> {
        let CreateRequest {
            id,
            template,
            spec,
            port,
            volume,
            net_index,
            from_tag,
            gateway_port,
            token,
            egress,
        } = *req;
        let rootfs = self.template_rootfs(template)?;
        let spec: TemplateSpec = serde_json::from_value(spec.clone())
            .with_context(|| format!("template {template} has an unusable spec"))?;
        if from_tag.is_some() {
            self.may_restore(id)?;
        }

        // A sandbox that was detached has its layer archived on the volume,
        // and "Create" is how it comes back: nothing on this node observes a
        // detached sandbox, and a cold start needs the template and port
        // anyway. The archive goes down before the overlay is mounted.
        let archive = self.archive_path(id);
        let restore_from = archive.exists().then_some(archive.as_path());
        if restore_from.is_some() {
            tracing::info!(%id, archive = %archive.display(), "restoring detached layer");
        }

        // Its own network namespace. Each sandbox is alone on a /30, so the
        // port it binds is its own business -- two sandboxes may both use
        // 8080, which is why no host-wide port check is needed any more.
        let mut veth = net::Pending::done();
        let (netns, sandbox_ip) = match net_index {
            Some(i) => {
                let (slot, pending) =
                    net::setup_started(&self.cfg.net, id, i, self.pool.as_deref())
                        .with_context(|| format!("setting up the network for {id}"))?;
                self.egress
                    .set(id, slot.sandbox_ip, egress)
                    .with_context(|| format!("setting what {id} may reach"))?;
                if self.engine.veth_needed_to_start() {
                    pending
                        .wait()
                        .with_context(|| format!("setting up the network for {id}"))?;
                } else {
                    veth = pending;
                }
                // Persisted so a restarted node can map this sandbox back to
                // its slot, which it needs to reclaim the address later.
                let bundles = self.cfg.work_dir.join("bundles").join(id);
                std::fs::create_dir_all(&bundles)?;
                std::fs::write(bundles.join("net-index"), i.to_string())?;
                // The gateway's port and the token it checks, beside the
                // net index and for the same reason: a restarted node must
                // be able to re-expose this sandbox, and a port that came
                // back open without its token would be worse than one that
                // did not come back at all.
                if let Some(gp) = gateway_port {
                    std::fs::write(bundles.join("gateway-port"), gp.to_string())?;
                }
                std::fs::write(bundles.join("sandbox-port"), port.to_string())?;
                if !token.is_empty() {
                    let tp = bundles.join("token");
                    std::fs::write(&tp, token)?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(&tp, std::fs::Permissions::from_mode(0o600))?;
                    }
                }
                tracing::info!(%id, ip = %slot.sandbox_ip, netns = %slot.netns, "network ready");
                (
                    Some(format!("/var/run/netns/{}", slot.netns)),
                    Some(slot.sandbox_ip),
                )
            }
            None => (None, None),
        };

        let bundle = self
            .engine
            .prepare(
                &self.cfg,
                &bundle::BuildRequest {
                    id,
                    template_rootfs: &rootfs,
                    spec: &spec,
                    port,
                    volume,
                    restore_from,
                    netns: netns.as_deref(),
                    sandbox_ip,
                },
            )
            .with_context(|| format!("building bundle for {id}"))?;

        tracing::info!(%id, %template, port, ?volume, "create");

        // A warm start: restore the template's booted image instead of
        // booting. Not with a volume -- the seed booted against an empty
        // one, and a program that read its volume at startup would carry
        // that view into a sandbox whose volume is not empty -- and never for
        // a seed itself.
        let warm = match (&spec.warm, from_tag, restore_from, volume) {
            (Some(w), None, None, None) if !id.starts_with(WARM_SEED_PREFIX) => {
                let dir = self.warm_dir(template, req.spec, port, &rootfs);
                self.distrust_warm_image(id);
                if dir.join("ready").exists() {
                    Some((dir, w.clone()))
                } else {
                    self.seed_later(template, req.spec, port, dir);
                    None
                }
            }
            _ => None,
        };
        if let Some((image, w)) = warm {
            let t0 = Instant::now();
            match self.restore_warm(id, &bundle, &image, &w) {
                Ok(()) => {
                    self.finish_net(id, std::mem::replace(&mut veth, net::Pending::done()))?;
                    self.accept_connections(id);
                    self.note_warm_start(id, image);
                    tracing::info!(%id, %template, ms = t0.elapsed().as_millis() as u64, "started warm");
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!(%id, %template, error = %format!("{e:#}"), "warm start failed; booting instead");
                    self.engine.remove(id);
                }
            }
        }

        match from_tag {
            // A fork: restore from the tag's image. One image serves any
            // number of children, so nothing is copied -- measured on bm,
            // five children from one 14 MiB image in 226 ms.
            Some(tag) => {
                let image = self.tag_path(tag)?;
                if !image.is_dir() {
                    bail!("tag {tag} is not on this node (no {})", image.display());
                }
                // The network `setup` built above is untouched, as for a
                // warm start (`restore_warm`).
                self.engine
                    .restore(id, &bundle, &image)
                    .with_context(|| format!("forking {id} from tag {tag}"))?;
            }
            None => self.engine.boot(id, &bundle)?,
        }
        self.finish_net(id, veth)?;
        if from_tag.is_some() {
            self.note_restored(id);
        }
        // Whatever it was before, it is running now.
        self.accept_connections(id);
        tracing::info!(%id, mem_mib = ?self.memory_mib(id), "running");
        Ok(())
    }

    /// The rest of a sandbox's network, which a restore does not wait for.
    ///
    /// Without it the sandbox runs where nothing can reach it, and a node
    /// that reports it running is never asked to start it again: on bare
    /// metal one child of 100 stayed that way until it was deleted. So it is
    /// removed, and the create fails and is retried.
    fn finish_net(&self, id: &str, veth: net::Pending) -> Result<()> {
        let done = veth.wait();
        if done.is_err() {
            self.engine.remove(id);
        }
        done.with_context(|| format!("setting up the network for {id}"))
    }

    fn archive_path(&self, id: &str) -> PathBuf {
        self.cfg
            .volume_root
            .join(id)
            .join("_archive")
            .join("layer.tar.zst")
    }

    /// Where this node keeps tag images, its own and fetched ones.
    pub fn tags_dir(&self) -> PathBuf {
        self.cfg.work_dir.join("tags")
    }

    /// Where a tag's checkpoint image lives on this node.
    ///
    /// The tag becomes a path component, so it must be a plain name for the
    /// same reason a template must.
    fn tag_path(&self, tag: &str) -> Result<PathBuf> {
        // Checked again on this side on purpose: this is the call that
        // joins the tag into a path, so it must not depend on a caller
        // having validated it.
        if !crate::tag::is_safe(tag) {
            bail!("tag {tag:?} is not a plain name");
        }
        Ok(self.tags_dir().join(tag))
    }

    /// Checkpoint a running sandbox into a tag, then put the parent back.
    ///
    /// runsc's checkpoint stops the container, so the parent is restored
    /// from the image it just wrote. Measured on bm: 33 ms to checkpoint
    /// and 45 ms to restore, so the parent pauses for well under a tenth of
    /// a second. A live fork with no pause is an RFC non-goal.
    pub fn branch(&self, id: &str, tag: &str) -> Result<u64> {
        let image = self.tag_path(tag)?;
        if image.exists() {
            bail!("tag {tag} already exists");
        }

        let bundle = self.cfg.work_dir.join("bundles").join(id);
        if !bundle.join("config.json").exists() {
            bail!("{id} has no bundle on this node");
        }

        // Thaw first if frozen: checkpointing a frozen task set hangs.
        let _ = self.freeze(id, false);
        self.checkpoint_into(id, &image, false)
            .with_context(|| format!("checkpointing {id} into tag {tag}"))?;

        // Where the engine can, the parent just carries on. Otherwise, or if
        // that fails, it is put back from the image it just produced, with
        // its address reapplied first.
        let resumed = self
            .engine
            .resume_after_checkpoint(id, &image)
            .unwrap_or_else(|e| {
                tracing::warn!(%id, error = %format!("{e:#}"), "could not resume after branching; restoring");
                false
            });
        if !resumed {
            self.engine.remove(id);
            let restored = self
                .ensure_net(id)
                .and_then(|()| self.engine.restore(id, &bundle, &image));
            if let Err(error) = restored {
                // The tag is not registered after a failed branch. Keep its
                // sealed image where reconciliation can find the parent.
                let snapshot = self.cfg.work_dir.join("snapshots").join(id);
                self.drop_snapshot(id);
                clear_attempts(&snapshot)?;
                publish(&image, &snapshot).with_context(|| {
                    format!("preserving {id} after failed branch restore: {error:#}")
                })?;
                return Err(error)
                    .with_context(|| format!("restoring {id} after branching it into {tag}"));
            }
        }

        let size = dir_size(&image);
        tracing::info!(%id, %tag, size_bytes = size, "branched");
        Ok(size)
    }

    fn note_warm_start(&self, id: &str, image: PathBuf) {
        let ready = std::fs::metadata(image.join("ready"))
            .and_then(|m| m.modified())
            .ok();
        let mut started = self.warm_started.lock();
        started.retain(|_, s| s.at.elapsed() < WARM_SURVIVAL);
        started.insert(
            id.to_string(),
            WarmStart {
                dir: image,
                ready,
                at: Instant::now(),
            },
        );
    }

    /// `id` is being created again: if it was started from a warm image a
    /// moment ago, it died of it, and so would every sandbox after it.
    fn distrust_warm_image(&self, id: &str) {
        let Some(s) = self.warm_started.lock().remove(id) else {
            return;
        };
        if s.at.elapsed() >= WARM_SURVIVAL {
            return;
        }
        // Still known to runsc as a stopped container, which it will not
        // restore or boot over.
        self.engine.remove(id);
        drop_warm_image(id, &s.dir, s.ready);
    }

    /// Where a template's warm image lives, keyed by everything that would
    /// make an image of it wrong: the spec as the master sent it, the port,
    /// this node's runsc and kernel, when the template's rootfs last changed,
    /// and when the host files it binds in last changed. A change to any of
    /// them is a new key, and so a new seed.
    fn warm_dir(
        &self,
        template: &str,
        spec: &serde_json::Value,
        port: u16,
        rootfs: &Path,
    ) -> PathBuf {
        use sha2::{Digest, Sha256};
        let fp = self.fingerprint();
        let mtime = |p: &Path| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos())
        };
        let mut h = Sha256::new();
        h.update(template.as_bytes());
        h.update(serde_json::to_vec(spec).unwrap_or_default());
        h.update(port.to_le_bytes());
        h.update(fp.as_bytes());
        h.update(mtime(rootfs).to_le_bytes());
        h.update(mtime(&rootfs.join("app")).to_le_bytes());
        if self.cfg.backend == BackendKind::Runsc {
            let binds = serde_json::from_value::<TemplateSpec>(spec.clone())
                .map(|s| s.ro_binds)
                .unwrap_or_default();
            h.update(binds_changed_at(&binds).to_le_bytes());
        }
        let key = hex::encode(&h.finalize()[..12]);
        self.cfg.work_dir.join("warm").join(template).join(key)
    }

    /// Only from `create`, straight after `net::setup`: nothing has run in
    /// the namespace yet, so there is nothing for `ensure_net` to put back.
    /// Re-applying it anyway was about eight `ip` processes, 25 ms of a
    /// 104 ms warm start on bm.
    fn restore_warm(&self, id: &str, bundle: &Path, image: &Path, w: &WarmSpec) -> Result<()> {
        self.engine
            .restore(id, bundle, image)
            .context("restoring the warm image")?;
        if !w.after_restore.is_empty() {
            let (_, stderr, code) = self
                .exec(id, &w.after_restore, AFTER_RESTORE_TIMEOUT)
                .context("running after_restore")?;
            if code != 0 {
                bail!("after_restore exited {code}: {}", stderr.trim());
            }
        }
        Ok(())
    }

    /// Take a template's warm image in the background, from a sandbox of
    /// its own, so no caller's request state can end up in it.
    ///
    /// One seed at a time on a node -- it uses the node's single reserved
    /// network slot -- and none while the node is short of memory.
    fn seed_later(&self, template: &str, spec: &serde_json::Value, port: u16, dir: PathBuf) {
        use std::sync::{Mutex, OnceLock};
        static BUSY: OnceLock<Mutex<()>> = OnceLock::new();
        let failed = dir.with_extension("failed");
        let recently_failed = std::fs::metadata(&failed)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age < WARM_RETRY);
        if recently_failed || host_pressure().0 < 1024 {
            return;
        }
        let Ok(guard) = BUSY.get_or_init(Mutex::default).try_lock() else {
            return;
        };
        drop(guard);
        let (rt, template, spec) = (self.clone(), template.to_string(), spec.clone());
        let spawned = std::thread::Builder::new()
            .name("sandstorm-warm-seed".into())
            .spawn(move || {
                let Ok(_one) = BUSY.get_or_init(Mutex::default).try_lock() else {
                    return;
                };
                if dir.join("ready").exists() {
                    return;
                }
                let t0 = Instant::now();
                match rt.seed(&template, &spec, port, &dir) {
                    Ok(mib) => tracing::info!(
                        %template, image_mib = mib, ms = t0.elapsed().as_millis() as u64,
                        "warm image ready; new sandboxes of this template start from it"
                    ),
                    Err(e) => {
                        tracing::warn!(%template, error = %format!("{e:#}"), "could not take a warm image; staying cold for now");
                        let _ = std::fs::create_dir_all(dir.parent().unwrap_or(&dir));
                        let _ = std::fs::write(&failed, format!("{e:#}\n"));
                    }
                }
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "could not start a warm seed");
        }
    }

    fn seed(&self, template: &str, spec: &serde_json::Value, port: u16, dir: &Path) -> Result<u64> {
        let w: WarmSpec = serde_json::from_value::<TemplateSpec>(spec.clone())?
            .warm
            .unwrap_or_default();
        let key = dir.file_name().unwrap_or_default().to_string_lossy();
        let seed = format!("{WARM_SEED_PREFIX}{}", &key[..key.len().min(12)]);
        let slot = self.cfg.net.reserved_index();
        // Whatever an interrupted seed left behind.
        let _ = self.destroy_with_net(&seed, Some(slot));

        let result = (|| {
            self.create(&CreateRequest {
                id: &seed,
                template,
                spec,
                port,
                volume: None,
                net_index: Some(slot),
                from_tag: None,
                gateway_port: None,
                token: "",
                egress: &Default::default(),
            })?;
            let ip = self.sandbox_ip(&seed).context("the seed has no address")?;
            let deadline = Instant::now() + Duration::from_secs(w.ready_timeout_secs);
            let addr = std::net::SocketAddr::from((ip, port));
            while std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_err() {
                if Instant::now() > deadline {
                    bail!(
                        "the seed was not listening on {port} after {}s",
                        w.ready_timeout_secs
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            // Let the program finish with the probe's connection, so the
            // image does not carry a half-handled socket.
            std::thread::sleep(Duration::from_millis(300));

            let tmp = dir.with_extension("tmp");
            let _ = std::fs::remove_dir_all(&tmp);
            std::fs::create_dir_all(&tmp)?;
            // Uncompressed on purpose: this image is restored on every
            // create, so it lives in the page cache, where an uncompressed
            // image restores fastest (138 ms against 696 on bm).
            self.engine
                .checkpoint(&seed, &tmp, false)
                .context("checkpointing the seed")?;
            std::fs::write(tmp.join("ready"), "")?;
            // Older images of this template are for a spec or rootfs that
            // no longer exists.
            if let Some(parent) = dir.parent() {
                for e in std::fs::read_dir(parent).into_iter().flatten().flatten() {
                    if e.path() != tmp {
                        let _ = std::fs::remove_dir_all(e.path());
                        let _ = std::fs::remove_file(e.path());
                    }
                }
            }
            std::fs::rename(&tmp, dir)?;
            Ok(dir_size(dir) >> 20)
        })();
        self.engine.remove(&seed);
        let _ = self.destroy_with_net(&seed, Some(slot));
        result
    }

    /// Remove a tag's image. Only called once the master knows nothing is
    /// forked from it.
    pub fn drop_tag(&self, tag: &str) -> Result<()> {
        let image = self.tag_path(tag)?;
        std::fs::remove_dir_all(&image).ok();
        tracing::info!(%tag, "tag removed");
        Ok(())
    }

    /// Where a template's rootfs lives on this node.
    ///
    /// The name becomes a path component, so it must be a plain name: a
    /// master that could send `../..` here would be choosing a directory on
    /// the node's disk.
    /// Where this node's backend reads template `name` from, there or not.
    pub fn template_root(&self, name: &str) -> Result<PathBuf> {
        if name.is_empty() {
            bail!("no template named");
        }
        if name.contains('/') || name.contains('\\') || name.contains("..") || name.starts_with('.')
        {
            bail!("template name {name:?} is not a plain name");
        }
        Ok(self
            .engine
            .template_root(&self.cfg.work_dir.join("templates").join(name)))
    }

    fn template_rootfs(&self, name: &str) -> Result<PathBuf> {
        let dir = self.template_root(name)?;
        if !dir.exists() {
            bail!(
                "template {name} is not installed on this node (no {})",
                dir.display()
            );
        }
        Ok(dir)
    }

    /// Where a sandbox is now, `None` when it is not on this node.
    pub fn tier_of(&self, id: &str) -> Result<Option<Tier>> {
        Ok(self
            .observe()?
            .into_iter()
            .find(|o| o.id == id)
            .map(|o| o.tier))
    }

    /// Move a sandbox to a tier, walking the intermediate legs if the
    /// master asked for a jump. Each leg is cheap; doing them one at a time
    /// is what keeps a demotion from stalling on a tar.
    pub fn set_tier(&self, id: &str, want: Tier) -> Result<()> {
        // Whether connections are refused follows the DESTINATION, not the
        // steps taken to reach it. Demoting Hot -> Snapshot passes through
        // Frozen and thaws again on the way, so deciding per step left a
        // window of tens of milliseconds where a sandbox about to be
        // checkpointed looked reachable and a connect would hang.
        //
        // Order matters in both directions, for different reasons. Refuse
        // BEFORE going cold: nothing should reach a sandbox that is being
        // taken down, and Detached tears the bundle down, which takes the
        // net index with it -- after that there is no address left to write
        // a rule for. Accept AFTER coming back: a sandbox that is not up
        // yet should refuse rather than hang.
        if !want.is_serving() {
            self.refuse_connections(id);
        }
        let settle = |rt: &Self| {
            if want.is_serving() {
                rt.accept_connections(id);
            }
            // Only when Frozen is where it is staying. Hot -> Snapshot
            // passes through Frozen, and squeezing a sandbox that is about
            // to be checkpointed only makes the checkpoint page it back in.
            if want == Tier::Frozen && rt.cfg.reclaim_frozen {
                reclaim_later(
                    id,
                    rt.cgroup_path(id),
                    rt.cfg.pin_in_zswap,
                    rt.cfg.reclaim_floor_mib,
                );
            }
        };
        for _ in 0..Tier::ORDER.len() {
            let Some(have) = self.tier_of(id)? else {
                bail!("{id} is not on this node");
            };
            if have == want {
                settle(self);
                return Ok(());
            }
            // Colder: one step. Warmer: straight back to hot.
            let next = if want > have {
                have.colder().unwrap_or(want)
            } else {
                Tier::Hot
            };
            self.step(id, have, next)
                .with_context(|| format!("{id}: {have} -> {next}"))?;
            if next == want {
                settle(self);
                return Ok(());
            }
        }
        bail!("{id} did not reach {want}")
    }

    fn step(&self, id: &str, have: Tier, want: Tier) -> Result<()> {
        match (have, want) {
            (Tier::Hot, Tier::Frozen) => self.freeze(id, true),
            (Tier::Frozen, Tier::Hot) => self.freeze(id, false),
            (Tier::Frozen, Tier::Snapshot) => {
                // Thaw first: checkpointing a frozen task set hangs.
                self.freeze(id, false)?;
                self.checkpoint(id)
            }
            (Tier::Hot, Tier::Snapshot) => self.checkpoint(id),
            (Tier::Snapshot, Tier::Hot) => self.restore(id),
            (Tier::Snapshot, Tier::Detached) => {
                self.detach(id)?;
                self.egress.clear(id)
            }
            // Nothing on this node observes a detached sandbox, so the
            // master issues Create for it instead; create() notices the
            // archive on the volume and unpacks it.
            (Tier::Detached, Tier::Hot) => bail!(
                "{id} is detached; it comes back via Create, which carries \
                 the template and port a cold start needs"
            ),
            (from, to) => bail!("no {from} -> {to} transition"),
        }
    }

    fn freeze(&self, id: &str, on: bool) -> Result<()> {
        let cg = self.cgroup_path(id);
        let p = cg.join("cgroup.freeze");
        std::fs::write(&p, if on { "1" } else { "0" })
            .with_context(|| format!("writing {}", p.display()))?;

        // The write returns before the tasks have settled, so wait for the
        // kernel to say so. Measured at 8-11 ms; a second is a generous
        // ceiling that still fails rather than hanging for ever.
        let want = if on { "frozen 1" } else { "frozen 0" };
        let events = cg.join("cgroup.events");
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if std::fs::read_to_string(&events)
                .map(|s| s.lines().any(|l| l.trim() == want))
                .unwrap_or(false)
            {
                break;
            }
            if Instant::now() > deadline {
                bail!("{id} did not report {want} within 1s");
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        tracing::info!(%id, frozen = on, "freeze");
        if self.cfg.reclaim_frozen && self.cfg.pin_in_zswap && !self.cfg.reclaim_hot && !on {
            // After the thaw, which is what a waiting caller needs first.
            // Back to the default, so a hot sandbox under real pressure can
            // still spill to disk rather than be stuck -- unless hot reclaim
            // is managing it, which wants compressed RAM only.
            let _ = std::fs::write(cg.join("memory.zswap.writeback"), "1");
        }
        Ok(())
    }

    /// Whether `snapshots/<id>` holds an image this node finished. A sealed
    /// image must still match its seal; one without a seal (written before
    /// images were sealed) is judged by the backend from its files.
    fn has_snapshot(&self, id: &str) -> bool {
        let image = self.cfg.work_dir.join("snapshots").join(id);
        match std::fs::read(image.join(SEAL)) {
            Ok(seal) => serde_json::from_slice::<BTreeMap<String, u64>>(&seal)
                .ok()
                .zip(image_files(&image).ok())
                .is_some_and(|(sealed, now)| sealed == now),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                image.is_dir() && self.engine.validate(&image).is_ok()
            }
            Err(_) => false,
        }
    }

    /// Checkpoint `id` into `dest`, which must not exist yet. The backend
    /// writes into a directory of the attempt's own beside `dest`; the image
    /// is checked, sealed, and only then renamed into place. So `dest` is
    /// either a whole image or absent, and a failed or timed-out checkpoint
    /// never reads as a snapshot or a tag.
    fn checkpoint_into(&self, id: &str, dest: &Path, compress: bool) -> Result<()> {
        let attempt = clear_attempts(dest)?.join(format!(
            "{}{}",
            attempt_prefix(dest),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&attempt)?;
        let done = (|| {
            self.engine.checkpoint(id, &attempt, compress)?;
            self.engine
                .validate(&attempt)
                .context("the checkpoint wrote an incomplete image")?;
            seal(&attempt)?;
            publish(&attempt, dest)
        })();
        if done.is_err() {
            // A timed-out writer may still be in there; the next attempt or
            // a destroy clears whatever this cannot.
            if let Err(e) = std::fs::remove_dir_all(&attempt) {
                tracing::warn!(%id, path = %attempt.display(), error = %e, "could not remove a failed checkpoint attempt");
            }
        }
        done
    }

    /// The image and every attempt at one.
    fn drop_snapshot(&self, id: &str) {
        let image = self.cfg.work_dir.join("snapshots").join(id);
        std::fs::remove_dir_all(&image).ok();
        clear_attempts(&image).ok();
    }

    fn checkpoint(&self, id: &str) -> Result<()> {
        self.forget_restore(id);
        // Only a running sandbox is checkpointed, so an image already here
        // is one it has moved on from: a restore that could not delete it.
        let dir = self.cfg.work_dir.join("snapshots").join(id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("clearing the stale image at {}", dir.display()))?;
        }
        self.checkpoint_into(id, &dir, self.cfg.compress_snapshots)
            .with_context(|| format!("checkpointing {id}"))?;
        // The process is gone; the writable layer stays, which is why
        // Snapshot frees memory but not disk.
        self.engine.remove(id);
        self.release_net(id);
        tracing::info!(%id, "checkpointed");
        Ok(())
    }

    fn restore(&self, id: &str) -> Result<()> {
        let dir = self.cfg.work_dir.join("snapshots").join(id);
        let bundle = self.cfg.work_dir.join("bundles").join(id);
        self.may_restore(id)?;
        self.ensure_net(id)?;
        if !bundle.join("config.json").exists() {
            bail!(
                "no bundle for {id} at {}; its layer is gone, so this is a \
                 detached sandbox and must be attached rather than restored",
                bundle.display()
            );
        }
        if !self.has_snapshot(id) {
            bail!("{id} has no complete checkpoint at {}", dir.display());
        }
        self.engine
            .restore(id, &bundle, &dir)
            .with_context(|| format!("restoring {id}"))?;
        // Running now, so `observe` goes by the cgroup and the image is
        // harmless. It stays until the sandbox has lasted: `runsc restore`
        // returns before the program has run, and this is its only copy.
        self.note_restored(id);
        self.accept_connections(id);
        tracing::info!(%id, "restored");
        Ok(())
    }

    fn restored_marker(&self, id: &str) -> PathBuf {
        self.cfg.work_dir.join("bundles").join(id).join(RESTORED)
    }

    fn note_restored(&self, id: &str) {
        if let Err(e) = std::fs::write(self.restored_marker(id), "") {
            tracing::warn!(%id, error = %e, "could not note a restore");
        }
    }

    /// Whether `id` may be restored now, having asked before. It was restored
    /// a moment ago, so if it is being restored again it did not last.
    ///
    /// An image holds a program laid out for the files it had mapped, and on
    /// gVisor those are the host's. Measured on bare metal with a library
    /// replaced under a snapshot: the restore succeeded, the program died,
    /// the image had already been deleted, and the master recreated the
    /// sandbox from its template. The caller got a 200 from a sandbox whose
    /// memory had gone. So the image stays, the sandbox reads as Snapshot
    /// again, and the master shows why while this refuses. It gets a real
    /// try once `RESTORE_SURVIVAL` has passed, so one a minute at most, and
    /// comes back on its own if the files have.
    fn may_restore(&self, id: &str) -> Result<()> {
        let marker = self.restored_marker(id);
        let Ok(at) = std::fs::metadata(&marker).and_then(|m| m.modified()) else {
            return Ok(());
        };
        if at.elapsed().is_ok_and(|age| age >= RESTORE_SURVIVAL) {
            // Known to runsc as a stopped container, which it will not
            // restore over.
            self.engine.remove(id);
            return Ok(());
        }
        bail!(
            "{id} died within {}s of being restored, so it is not being restored \
             again yet. Its image is kept. The likely cause is that files bound in \
             from the host, such as libraries, changed since it was taken.",
            RESTORE_SURVIVAL.as_secs()
        )
    }

    /// Sandboxes that have lasted `min_age` since a restore, so what kept
    /// them safe can go.
    pub fn survived_restores(&self, min_age: Duration) -> Vec<String> {
        self.restored_before(min_age, |id| {
            std::fs::read_to_string(self.cgroup_path(id).join("cgroup.procs"))
                .is_ok_and(|s| !s.trim().is_empty())
        })
    }

    fn restored_before(&self, min_age: Duration, running: impl Fn(&str) -> bool) -> Vec<String> {
        let Ok(rd) = std::fs::read_dir(self.cfg.work_dir.join("bundles")) else {
            return Vec::new();
        };
        rd.flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|id| {
                std::fs::metadata(self.restored_marker(id))
                    .and_then(|m| m.modified())
                    .is_ok_and(|at| at.elapsed().is_ok_and(|age| age >= min_age))
            })
            .filter(|id| running(id))
            .collect()
    }

    /// `id` has lasted, or is about to be checkpointed: the image it came
    /// from, if it is its own, has nothing left to do.
    pub fn forget_restore(&self, id: &str) {
        if std::fs::remove_file(self.restored_marker(id)).is_ok() {
            self.drop_snapshot(id);
        }
    }

    /// Everything local goes into one archive on the volume. Measured on bm
    /// with 30k files: one tar+zstd round trip is 16.1 s against 57 s for
    /// per-file copies onto the volume, because the volume pays per file.
    fn detach(&self, id: &str) -> Result<()> {
        self.detach_with_commands(id, &mut Exec::new("tar"), &mut Exec::new("zstd"))
    }

    fn detach_with_commands(&self, id: &str, tar: &mut Exec, zstd: &mut Exec) -> Result<()> {
        // Unmount first, then archive: by here the sandbox is checkpointed
        // and gone, so nothing needs the layer, and reading a live one is
        // best avoided.
        self.engine.unmount(&self.cfg, id)?;

        // Only what this sandbox wrote: the template under it would be the
        // same bytes for every sandbox on the node.
        let layer = self.engine.layer(&self.cfg, id);
        let arc_dir = self.cfg.volume_root.join(id).join("_archive");
        std::fs::create_dir_all(&arc_dir)?;
        let arc = arc_dir.join("layer.tar.zst");

        // Keep the published archive untouched until BOTH pipeline stages succeed.
        let temporary = arc_dir.join(format!(".layer-{}.tar.zst.tmp", uuid::Uuid::new_v4()));
        let output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .context("creating temporary layer archive")?;
        let archived = (|| -> Result<()> {
            let compressed = output.try_clone().context("opening archive output")?;
            let mut tar = tar
                .arg("-C")
                .arg(&layer)
                // Opaque directories and whiteouts must still hide the template after restore.
                .args([
                    "--xattrs",
                    "--xattrs-include=trusted.overlay.*",
                    "--xattrs-include=user.overlay.*",
                    "-S",
                    "-cf",
                    "-",
                    ".",
                ])
                .stdout(Stdio::piped())
                .spawn()
                .context("starting tar to archive layer")?;
            let input = tar.stdout.take().expect("tar stdout is piped");
            let compressed_child = zstd
                .args(["-3", "-T0", "-q", "-c"])
                .stdin(Stdio::from(input))
                .stdout(Stdio::from(compressed))
                .spawn();
            // Command retains its configured pipe handles after spawn. Drop our
            // read end so a failed compressor cannot leave tar blocked writing.
            zstd.stdin(Stdio::null()).stdout(Stdio::inherit());
            let mut zstd = match compressed_child {
                Ok(child) => child,
                Err(error) => {
                    let _ = tar.kill();
                    tar.wait()
                        .context("reaping tar after zstd failed to start")?;
                    return Err(error).context("starting zstd to archive layer");
                }
            };
            // Always reap both children, even when one stage fails.
            let tar_status = tar.wait();
            if tar_status.is_err() {
                let _ = tar.kill();
                let _ = tar.wait();
            }
            let zstd_status = zstd.wait();
            if zstd_status.is_err() {
                let _ = zstd.kill();
                let _ = zstd.wait();
            }
            let tar_status = tar_status.context("waiting for tar to archive layer")?;
            let zstd_status = zstd_status.context("waiting for zstd to archive layer")?;
            if !tar_status.success() || !zstd_status.success() {
                bail!("archiving {id} failed: tar {tar_status}; zstd {zstd_status}");
            }
            output
                .sync_all()
                .context("syncing completed layer archive")?;
            std::fs::rename(&temporary, &arc).context("publishing completed layer archive")?;
            // The local copy is deleted next; the rename has to be on disk first.
            std::fs::File::open(&arc_dir)
                .and_then(|d| d.sync_all())
                .context("syncing the archive directory")?;
            Ok(())
        })();
        drop(output);
        if let Err(error) = archived {
            std::fs::remove_file(&temporary).context(format!(
                "removing failed temporary archive after: {error:#}"
            ))?;
            return Err(error);
        }
        self.drop_snapshot(id);
        // Unmount and drop the layer: detached means nothing local at all.
        self.engine.teardown(&self.cfg, id)?;
        tracing::info!(%id, archive = %arc.display(), "detached");
        Ok(())
    }

    /// Run a command inside a running sandbox. See `Backend::exec`.
    pub fn exec(
        &self,
        id: &str,
        args: &[String],
        timeout: Duration,
    ) -> Result<(String, String, i64)> {
        self.engine.exec(id, args, timeout)
    }

    /// Free a sandbox's address now, ahead of a `destroy_with_net` that will
    /// run later; see `net::quiesce`.
    /// Replace what a sandbox may reach, in any tier: rules follow its
    /// address, which it keeps while it exists.
    pub fn set_egress(&self, id: &str, egress: &crate::proto::Egress) -> Result<()> {
        let index = self
            .net_index_of(id)
            .with_context(|| format!("{id} has no address on this node"))?;
        self.egress
            .set(id, self.cfg.net.slot(id, index)?.sandbox_ip, egress)
    }

    pub fn quiesce_net(&self, id: &str, net_index: Option<u32>) {
        if let Some(i) = net_index.or_else(|| self.net_index_of(id)) {
            let _ = net::quiesce(&self.cfg.net, id, i);
        }
    }

    pub fn destroy_with_net(&self, id: &str, net_index: Option<u32>) -> Result<()> {
        // Prefer our own record: the master may be asking about an orphan it
        // no longer has a row for.
        let index = net_index.or_else(|| self.net_index_of(id));
        // Read the address before teardown removes what it is derived from,
        // or the rule outlives the sandbox and the next holder of that slot
        // is refused.
        let ip = index
            .and_then(|i| self.cfg.net.slot(id, i).ok())
            .map(|s| s.sandbox_ip);
        self.destroy(id)?;
        self.egress
            .clear(id)
            .with_context(|| format!("removing what {id} was allowed to reach"))?;
        if let Some(i) = index {
            net::teardown(&self.cfg.net, id, i, self.pool.as_deref())
                .with_context(|| format!("tearing down the network for {id}"))?;
        }
        if let Some(ip) = ip {
            if let Err(e) = net::allow(ip) {
                tracing::warn!(%id, error = %e, "could not clear the cold rule");
            }
        }
        Ok(())
    }

    /// What this sandbox should be exposed as, read back from its bundle.
    ///
    /// Returns `None` when either half is missing: a port with no token
    /// would be an open door, and a token with no port has nothing to
    /// guard, so neither is served on its own.
    pub fn exposed(&self, id: &str) -> Option<crate::node::Exposed> {
        let dir = self.cfg.work_dir.join("bundles").join(id);
        let gateway_port: u16 = std::fs::read_to_string(dir.join("gateway-port"))
            .ok()?
            .trim()
            .parse()
            .ok()?;
        let token = std::fs::read_to_string(dir.join("token")).ok()?;
        let token = token.trim().to_string();
        if token.is_empty() {
            return None;
        }
        Some(crate::node::Exposed {
            id: id.to_string(),
            gateway_port,
            sandbox_ip: self.sandbox_ip(id)?,
            sandbox_port: self.sandbox_port_of(id).unwrap_or(8080),
            token,
        })
    }

    /// The port the sandbox's process listens on inside its namespace,
    /// recorded at create time because the gateway needs to forward to it.
    fn sandbox_port_of(&self, id: &str) -> Option<u16> {
        std::fs::read_to_string(
            self.cfg
                .work_dir
                .join("bundles")
                .join(id)
                .join("sandbox-port"),
        )
        .ok()?
        .trim()
        .parse()
        .ok()
    }

    fn net_index_of(&self, id: &str) -> Option<u32> {
        std::fs::read_to_string(self.cfg.work_dir.join("bundles").join(id).join("net-index"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// Take a snapshotted sandbox's namespace down, keeping its slot; see
    /// `net::release`. The per-address REJECT goes with it: with no route
    /// left, the unreachable floor refuses faster than the rule did, and a
    /// rule per snapshotted sandbox was one more line every outgoing packet
    /// on the host walked past. `restore` rebuilds both through
    /// `ensure_net`. A failure is logged, not returned: the snapshot itself
    /// succeeded, and a namespace left up costs memory, not correctness.
    fn release_net(&self, id: &str) {
        let Some(index) = self.net_index_of(id) else {
            return;
        };
        let ip = self.sandbox_ip(id);
        if let Err(e) = net::release(&self.cfg.net, id, index, self.pool.as_deref()) {
            tracing::warn!(%id, error = %e, "could not release the network of a snapshotted sandbox");
            return;
        }
        if let Some(ip) = ip {
            let _ = net::allow(ip);
        }
    }

    /// Refuse connections to this sandbox, because it is not running.
    ///
    /// Called on every path out of Hot. A failure here is logged and not
    /// propagated: the tier change itself succeeded, and the consequence
    /// of a missing rule is a caller waiting out a timeout rather than
    /// anything incorrect.
    fn refuse_connections(&self, id: &str) {
        if let Some(ip) = self.sandbox_ip(id) {
            if let Err(e) = net::reject(ip) {
                tracing::warn!(%id, error = %e, "could not refuse connections while cold");
            }
        }
    }

    /// Accept connections again. Called on every path into Hot, including
    /// ones that were never cold, since it is idempotent.
    fn accept_connections(&self, id: &str) {
        if let Some(ip) = self.sandbox_ip(id) {
            if let Err(e) = net::allow(ip) {
                tracing::warn!(%id, error = %e, "could not re-allow connections");
            }
        }
    }

    fn sandbox_ip(&self, id: &str) -> Option<std::net::Ipv4Addr> {
        let i = self.net_index_of(id)?;
        self.cfg.net.slot(id, i).ok().map(|s| s.sandbox_ip)
    }

    /// Put the sandbox's address back before restoring it.
    ///
    /// gVisor takes the veth device over, and deleting the container leaves
    /// it in the namespace with its address gone -- the host side keeps
    /// 10.86.x.1/30 while the sandbox side has no inet at all. A restore
    /// then re-attaches to an unconfigured device, so the sandbox comes
    /// back running but unreachable. `net::setup` is idempotent, so this
    /// simply re-applies the plan.
    ///
    /// Every restore path needs it: the snapshot tier coming back to hot,
    /// and a branch putting its parent back.
    fn ensure_net(&self, id: &str) -> Result<()> {
        let Some(i) = self.net_index_of(id) else {
            return Ok(());
        };
        let slot = net::setup(&self.cfg.net, id, i, self.pool.as_deref())
            .with_context(|| format!("restoring the network for {id}"))?;
        tracing::debug!(%id, ip = %slot.sandbox_ip, "network reapplied before restore");
        Ok(())
    }

    /// The address this sandbox was given, for the master's records.
    fn ip_of(&self, id: &str) -> Option<String> {
        let i = self.net_index_of(id)?;
        self.cfg
            .net
            .slot(id, i)
            .ok()
            .map(|s| s.sandbox_ip.to_string())
    }

    pub fn destroy(&self, id: &str) -> Result<()> {
        // Thaw first: a frozen cgroup's tasks cannot be killed at all. On bm
        // this left gvisor_sentry processes that nothing could reap and that
        // went on holding their sandbox's port.
        let _ = self.freeze(id, false);
        self.engine.kill(id);
        self.engine.remove(id);

        // Backstop: the backend's own kill can leave something running --
        // runsc's can leave the sentry, certainly when its state for the
        // container is already gone. cgroup.kill takes the whole cgroup
        // regardless of what the backend thinks.
        let cg = self.cgroup_path(id);
        if cg.exists() {
            if std::fs::write(cg.join("cgroup.kill"), "1").is_ok() {
                // The kill is asynchronous; wait for the cgroup to empty so
                // the port is really free before we report success.
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline {
                    let empty = std::fs::read_to_string(cg.join("cgroup.procs"))
                        .map(|s| s.trim().is_empty())
                        .unwrap_or(true);
                    if empty {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            std::fs::remove_dir(&cg).ok();
        }
        self.drop_snapshot(id);
        self.engine.teardown(&self.cfg, id)?;
        // The archived layer is ours; the volume's /workspace is the user's
        // and is left alone.
        if let Some(d) = self.archive_path(id).parent() {
            std::fs::remove_dir_all(d).ok();
        }
        tracing::info!(%id, "destroyed");
        Ok(())
    }
}

/// The fingerprint alone; see `host`.
#[cfg(test)]
pub fn fingerprint(engine_version: &str) -> String {
    host(engine_version).0
}

/// What this node is snapshot-compatible with, and what that is made of.
///
/// Cross-node restore failed on bj -> hk even with every one of these equal
/// (`killed by signal 11` after a restore that reported success), so a match
/// is necessary and NOT sufficient. This gates placement; continuity across
/// nodes comes from the volume, not from moving a snapshot.
///
/// `engine_version` is `Backend::version`: an image is only as portable as
/// the engine that wrote it.
///
/// Both come from one read of the host, so they can never describe
/// different moments. The hash is over the raw `vendor_id` and `flags`
/// lines, as it always has been: changing it would move every node out of
/// its pool at once and strand every tag.
pub fn host(engine_version: &str) -> (String, HostInfo) {
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let line = |key: &str| {
        cpuinfo
            .lines()
            .find(|l| l.starts_with(key))
            .unwrap_or("")
            .to_string()
    };
    let (flags, vendor, model) = (line("flags"), line("vendor_id"), line("model name"));

    let mut h = 0xcbf29ce484222325u64;
    for b in format!("{engine_version}|{}|{vendor}|{flags}", kernel.trim()).bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    let value = |l: &str| l.split_once(':').map_or("", |(_, v)| v).trim().to_string();
    let info = HostInfo {
        engine: engine_version.trim().to_string(),
        kernel: kernel.trim().to_string(),
        cpu_vendor: value(&vendor),
        cpu_model: value(&model),
        cpu_flags: value(&flags),
    };
    (format!("{h:016x}"), info)
}

/// Free memory and the 10-second "some" memory-pressure average.
///
/// PSI is the signal that actually predicts trouble: on bm, free memory
/// stayed healthy-looking while 11 of 12 cold-tier wakeups timed out,
/// because the cost was reclaim contention rather than exhaustion.
pub fn host_pressure() -> (u64, f64) {
    let free_mib = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemAvailable:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
        })
        .map(|kb| kb / 1024)
        .unwrap_or(0);

    let psi = std::fs::read_to_string("/proc/pressure/memory")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("some"))?
                .split_whitespace()
                .find_map(|f| f.strip_prefix("avg10=")?.parse::<f64>().ok())
        })
        .unwrap_or(0.0);

    (free_mib, psi)
}

/// What admission reads off the host: `host_pressure`, and the swap left.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HostReading {
    /// MemAvailable. 0 when /proc/meminfo could not be read.
    pub free_mib: u64,
    pub swap_free_mib: u64,
    /// 0 with no swap at all, or none that could be read.
    pub swap_total_mib: u64,
    pub psi: f64,
}

impl From<(u64, f64)> for HostReading {
    /// A reading with no swap, which is what every test before swap was
    /// read stated.
    fn from((free_mib, psi): (u64, f64)) -> Self {
        Self {
            free_mib,
            swap_free_mib: 0,
            swap_total_mib: 0,
            psi,
        }
    }
}

impl HostReading {
    /// The swap a node keeps back: its memory floor, or a quarter of its
    /// swap where that is less, so a small swap file does not leave a node
    /// permanently short. `None` with no swap.
    pub fn swap_floor_mib(&self, min_free_mib: u64) -> Option<u64> {
        (self.swap_total_mib > 0).then(|| min_free_mib.min(self.swap_total_mib / 4))
    }

    pub fn swap_short(&self, min_free_mib: u64) -> bool {
        self.swap_floor_mib(min_free_mib)
            .is_some_and(|floor| self.swap_free_mib < floor)
    }
}

/// Swap is a floor of its own, beside free memory.
///
/// Where swap is zswap, a page squeezed out of a sandbox costs little RAM
/// but a whole swap slot -- with writeback off it never leaves RAM, but it
/// still holds its slot. Pages that compress well use the slots up while
/// MemAvailable looks healthy: on 2026-10-28 f8's heaps were `numpy.ones`,
/// its 2 GiB of swap ran out with 678 MiB "available", nothing anonymous
/// could be reclaimed any more, and the kernel thrashed on page cache until
/// the node stopped answering ssh for an hour and a half.
pub fn host_reading() -> HostReading {
    let (free_mib, psi) = host_pressure();
    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let field = |name: &str| {
        meminfo
            .lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
            .map_or(0, |kb| kb / 1024)
    };
    HostReading {
        free_mib,
        swap_free_mib: field("SwapFree:"),
        swap_total_mib: field("SwapTotal:"),
        psi,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_stable_within_a_host() {
        let a = fingerprint("runsc version release-20261019.0");
        let b = fingerprint("runsc version release-20261019.0");
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        // An image is only as portable as the engine that wrote it.
        assert_ne!(a, fingerprint("runsc version release-20261026.0"));
    }

    /// The parts are read alongside the hash, never instead of it: the hash
    /// is the one already recorded for every node and tag.
    #[test]
    fn the_host_parts_come_with_the_same_fingerprint() {
        let (fp, info) = host("runsc version release-20261019.0");
        assert_eq!(fp, fingerprint("runsc version release-20261019.0"));
        assert_eq!(info.engine, "runsc version release-20261019.0");
        assert!(
            !info.cpu_flags.contains(':'),
            "the label is stripped: {}",
            info.cpu_flags
        );
    }

    /// Demotion must never skip a tier; the master walks one step per sweep.
    #[test]
    fn no_direct_hot_to_detached() {
        let rt = Runtime::new(RuntimeConfig {
            work_dir: std::env::temp_dir().join("sandstorm-rt-test"),
            volume_root: std::env::temp_dir().join("sandstorm-rt-test/vol"),
            ..Default::default()
        })
        .unwrap();
        // observe() finds nothing for an unknown id, so this reports the
        // more basic error first; the transition table itself is asserted
        // by the match arms being exhaustive.
        let e = rt.set_tier("nope", Tier::Detached).unwrap_err().to_string();
        assert!(e.contains("not on this node"), "got: {e}");
    }
}

#[cfg(test)]
mod backend_tests {
    use super::*;
    use parking_lot::Mutex;
    use tempfile::TempDir;

    /// Records what it was asked to do, and writes an image so a checkpoint
    /// leaves something behind -- unless `.1` is set, when it writes nothing
    /// and still reports success, as a checkpoint that went wrong quietly.
    #[derive(Default)]
    struct Recorder(
        Mutex<Vec<String>>,
        std::sync::atomic::AtomicBool,
        std::sync::atomic::AtomicBool,
    );

    impl Recorder {
        fn log(&self, s: String) {
            self.0.lock().push(s);
        }
    }

    impl Backend for Recorder {
        fn version(&self) -> String {
            "recorder 1".into()
        }
        fn template_root(&self, dir: &Path) -> PathBuf {
            dir.join("rootfs")
        }
        fn prepare(&self, cfg: &RuntimeConfig, req: &bundle::BuildRequest<'_>) -> Result<PathBuf> {
            let b = cfg.work_dir.join("bundles").join(req.id);
            std::fs::create_dir_all(&b)?;
            std::fs::write(b.join("config.json"), "{}")?;
            self.log(format!("prepare {}", req.id));
            Ok(b)
        }
        fn layer(&self, cfg: &RuntimeConfig, id: &str) -> PathBuf {
            cfg.work_dir.join("layers").join(id)
        }
        fn unmount(&self, _: &RuntimeConfig, _: &str) -> Result<()> {
            Ok(())
        }
        fn teardown(&self, cfg: &RuntimeConfig, id: &str) -> Result<()> {
            std::fs::remove_dir_all(cfg.work_dir.join("bundles").join(id)).ok();
            Ok(())
        }
        fn boot(&self, id: &str, _: &Path) -> Result<()> {
            self.log(format!("boot {id}"));
            Ok(())
        }
        fn restore(&self, id: &str, _: &Path, image: &Path) -> Result<()> {
            self.log(format!(
                "restore {id} {}",
                image.file_name().unwrap().to_string_lossy()
            ));
            if self.2.swap(false, std::sync::atomic::Ordering::Relaxed) {
                bail!("injected restore failure");
            }
            Ok(())
        }
        fn checkpoint(&self, id: &str, image: &Path, compress: bool) -> Result<()> {
            // The attempt directory's name is random, so it is not logged.
            if !self.1.load(std::sync::atomic::Ordering::Relaxed) {
                std::fs::write(image.join("checkpoint.img"), b"pages")?;
            }
            self.log(format!("checkpoint {id} {compress}"));
            Ok(())
        }
        fn validate(&self, image: &Path) -> Result<()> {
            if !image.join("checkpoint.img").is_file() {
                bail!("no checkpoint.img");
            }
            Ok(())
        }
        fn remove(&self, id: &str) {
            self.log(format!("remove {id}"));
        }
        fn kill(&self, id: &str) {
            self.log(format!("kill {id}"));
        }
        fn exec(&self, id: &str, args: &[String], _: Duration) -> Result<(String, String, i64)> {
            self.log(format!("exec {id} {}", args.join(" ")));
            Ok((String::new(), String::new(), 0))
        }
    }

    /// It would otherwise run where nothing can reach it, and be reported as
    /// running, so never started again.
    #[test]
    fn a_sandbox_whose_network_does_not_arrive_is_removed() {
        let dir = TempDir::new().unwrap();
        let rec = Arc::new(Recorder::default());
        let rt = Runtime::with_backend(
            RuntimeConfig {
                work_dir: dir.path().to_path_buf(),
                volume_root: dir.path().join("volumes"),
                ..Default::default()
            },
            rec.clone(),
        )
        .unwrap();

        rt.finish_net("sb", net::Pending::done()).unwrap();
        assert!(rec.0.lock().is_empty());

        let e = rt.finish_net("sb", net::Pending::failed()).unwrap_err();
        assert!(format!("{e:#}").contains("the veth has gone"), "{e:#}");
        assert_eq!(*rec.0.lock(), ["remove sb"]);
    }

    /// It was seen on bare metal as every create starting twice, the second
    /// time cold, with nothing to say why.
    #[test]
    fn a_sandbox_that_dies_after_a_warm_start_takes_the_image_with_it() {
        let dir = TempDir::new().unwrap();
        let rec = Arc::new(Recorder::default());
        let rt = Runtime::with_backend(
            RuntimeConfig {
                work_dir: dir.path().to_path_buf(),
                volume_root: dir.path().join("volumes"),
                ..Default::default()
            },
            rec.clone(),
        )
        .unwrap();
        let rootfs = dir.path().join("templates/py/rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let spec = serde_json::json!({"args": ["x"], "warm": {}, "ro_binds": []});
        let image = rt.warm_dir("py", &spec, 8080, &rootfs);
        std::fs::create_dir_all(&image).unwrap();
        std::fs::write(image.join("ready"), "").unwrap();
        let create = || {
            rt.create(&CreateRequest {
                id: "sb",
                template: "py",
                spec: &spec,
                port: 8080,
                volume: None,
                net_index: None,
                from_tag: None,
                gateway_port: None,
                token: "",
                egress: &Default::default(),
            })
        };

        create().unwrap();
        assert!(rec.0.lock().iter().any(|l| l.starts_with("restore sb")));
        rec.0.lock().clear();

        create().unwrap();
        assert!(!image.exists(), "the image is gone");
        let log = rec.0.lock().clone();
        assert_eq!(log, ["prepare sb", "remove sb", "boot sb"][..]);
    }

    fn restored_rt(dir: &TempDir) -> (Runtime, Arc<Recorder>, PathBuf) {
        let rec = Arc::new(Recorder::default());
        let rt = Runtime::with_backend(
            RuntimeConfig {
                work_dir: dir.path().to_path_buf(),
                volume_root: dir.path().join("volumes"),
                ..Default::default()
            },
            rec.clone(),
        )
        .unwrap();
        let bundle = dir.path().join("bundles/sb");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("config.json"), "{}").unwrap();
        let image = dir.path().join("snapshots/sb");
        std::fs::create_dir_all(&image).unwrap();
        std::fs::write(image.join("checkpoint.img"), b"pages").unwrap();
        (rt, rec, image)
    }

    fn age_marker(rt: &Runtime, id: &str, by: Duration) {
        let f = std::fs::File::options()
            .write(true)
            .open(rt.restored_marker(id))
            .unwrap();
        f.set_modified(std::time::SystemTime::now() - by).unwrap();
    }

    /// `runsc restore` returns before the program has run, so the restore
    /// succeeding says nothing about whether the sandbox will last.
    #[test]
    fn an_image_outlasts_the_restore_from_it() {
        let dir = TempDir::new().unwrap();
        let (rt, _, image) = restored_rt(&dir);

        rt.restore("sb").unwrap();

        assert!(image.join("checkpoint.img").exists());
    }

    /// Seen on bare metal with a library replaced under a snapshot: the
    /// memory went, and the caller was told nothing.
    #[test]
    fn a_sandbox_that_dies_after_a_restore_keeps_its_image_and_says_so() {
        let dir = TempDir::new().unwrap();
        let (rt, rec, image) = restored_rt(&dir);
        rt.restore("sb").unwrap();
        rec.0.lock().clear();

        let e = rt.restore("sb").unwrap_err();

        assert!(format!("{e:#}").contains("Its image is kept"), "{e:#}");
        assert!(rec.0.lock().is_empty(), "no second try");
        assert!(image.join("checkpoint.img").exists());
    }

    /// For when the files are back, and at a pace that costs nothing if not.
    #[test]
    fn a_later_restore_gets_another_try() {
        let dir = TempDir::new().unwrap();
        let (rt, rec, _) = restored_rt(&dir);
        rt.restore("sb").unwrap();
        rec.0.lock().clear();
        age_marker(&rt, "sb", RESTORE_SURVIVAL);

        rt.restore("sb").unwrap();

        assert_eq!(rec.0.lock()[0], "remove sb", "the stopped container first");
        assert!(rec.0.lock()[1].starts_with("restore sb"));
    }

    #[test]
    fn the_image_goes_once_the_sandbox_has_lasted() {
        let dir = TempDir::new().unwrap();
        let (rt, _, image) = restored_rt(&dir);
        rt.restore("sb").unwrap();

        let running = |_: &str| true;
        assert!(rt.restored_before(RESTORE_SURVIVAL, running).is_empty());
        age_marker(&rt, "sb", RESTORE_SURVIVAL);
        assert!(rt.restored_before(RESTORE_SURVIVAL, |_| false).is_empty());
        assert_eq!(rt.restored_before(RESTORE_SURVIVAL, running), ["sb"]);

        rt.forget_restore("sb");
        assert!(!image.exists());
        assert!(!rt.restored_marker("sb").exists());
    }

    /// A sandbox put away again inside the window is a new image, not the
    /// one it came from.
    #[test]
    fn a_new_checkpoint_is_not_taken_for_the_old_image() {
        let dir = TempDir::new().unwrap();
        let (rt, _, image) = restored_rt(&dir);
        rt.restore("sb").unwrap();

        rt.checkpoint("sb").unwrap();

        assert!(!rt.restored_marker("sb").exists());
        assert!(image.exists());
        rt.restore("sb").unwrap();
    }

    /// Branching is the one sequence that must come out in this order: the
    /// parent is stopped by its own checkpoint and put back from the image
    /// it just wrote, uncompressed whatever the node's setting, since every
    /// fork restores from it.
    #[test]
    fn a_branch_checkpoints_removes_and_restores_the_parent_from_the_tag() {
        let dir = TempDir::new().unwrap();
        let rec = Arc::new(Recorder::default());
        let rt = Runtime::with_backend(
            RuntimeConfig {
                work_dir: dir.path().to_path_buf(),
                volume_root: dir.path().join("volumes"),
                compress_snapshots: true,
                ..Default::default()
            },
            rec.clone(),
        )
        .unwrap();
        let bundle = dir.path().join("bundles").join("sb1");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("config.json"), "{}").unwrap();

        // The image and its seal.
        let size = rt.branch("sb1", "t1").unwrap();
        assert_eq!(size, dir_size(&dir.path().join("tags/t1")));
        assert!(size > 5);
        assert_eq!(
            *rec.0.lock(),
            ["checkpoint sb1 false", "remove sb1", "restore sb1 t1"]
        );
        // A tag that exists is never overwritten.
        assert!(rt.branch("sb1", "t1").is_err());
    }

    #[test]
    fn failed_branch_restore_keeps_parent_checkpoint_for_reconciliation() {
        let dir = TempDir::new().unwrap();
        let (rec, rt) = rec_rt(dir.path());
        rec.2.store(true, std::sync::atomic::Ordering::Relaxed);

        let error = rt.branch("sb1", "t1").unwrap_err();
        assert!(format!("{error:#}").contains("injected restore failure"));
        assert!(rt.has_snapshot("sb1"));
        assert_eq!(
            std::fs::read(dir.path().join("snapshots/sb1/checkpoint.img")).unwrap(),
            b"pages"
        );
        assert_eq!(rt.observe().unwrap()[0].tier, Tier::Snapshot);
        rt.drop_tag("t1").unwrap();
        assert!(rt.has_snapshot("sb1"));

        let restarted = Runtime::with_backend(
            RuntimeConfig {
                work_dir: dir.path().to_path_buf(),
                volume_root: dir.path().join("volumes"),
                ..Default::default()
            },
            rec.clone(),
        )
        .unwrap();
        assert_eq!(restarted.observe().unwrap()[0].tier, Tier::Snapshot);
        restarted.set_tier("sb1", Tier::Hot).unwrap();
        restarted.forget_restore("sb1");
        assert!(!restarted.has_snapshot("sb1"));
        assert_eq!(rec.0.lock().last().unwrap(), "restore sb1 sb1");
        assert!(!rec.0.lock().iter().any(|call| call.starts_with("boot ")));
    }

    fn rec_rt(dir: &Path) -> (Arc<Recorder>, Runtime) {
        let rec = Arc::new(Recorder::default());
        let rt = Runtime::with_backend(
            RuntimeConfig {
                work_dir: dir.to_path_buf(),
                volume_root: dir.join("volumes"),
                ..Default::default()
            },
            rec.clone(),
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("bundles/sb1")).unwrap();
        std::fs::write(dir.join("bundles/sb1/config.json"), "{}").unwrap();
        (rec, rt)
    }

    /// A checkpoint that reports success but leaves no usable image is not
    /// published: no snapshot, no attempt left over, and the sandbox is not
    /// stopped or reported as Snapshot, so the master can try again.
    #[test]
    fn a_checkpoint_without_an_image_is_not_a_snapshot() {
        let dir = TempDir::new().unwrap();
        let (rec, rt) = rec_rt(dir.path());
        rec.1.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(rt.checkpoint("sb1").is_err());
        assert_eq!(*rec.0.lock(), ["checkpoint sb1 false"]);
        let snapshots = dir.path().join("snapshots");
        assert_eq!(std::fs::read_dir(&snapshots).unwrap().count(), 0);
        assert!(!rt.has_snapshot("sb1"));
        assert!(rt.observe().unwrap().is_empty());

        // The next attempt goes through, and replaces nothing.
        rec.1.store(false, std::sync::atomic::Ordering::Relaxed);
        rt.checkpoint("sb1").unwrap();
        assert!(rt.has_snapshot("sb1"));
        let names: Vec<_> = std::fs::read_dir(&snapshots)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["sb1"]);
    }

    /// A sealed image whose files have changed since is not one this node
    /// finished; a tag the backend later adds a dot-named cache to still is.
    #[test]
    fn a_sealed_image_must_still_match_its_seal() {
        let dir = TempDir::new().unwrap();
        let (_, rt) = rec_rt(dir.path());
        rt.checkpoint("sb1").unwrap();
        let image = dir.path().join("snapshots/sb1");
        std::fs::write(image.join(".mem.assembled"), b"cache").unwrap();
        assert!(rt.has_snapshot("sb1"));
        std::fs::write(image.join("checkpoint.img"), b"pag").unwrap();
        assert!(!rt.has_snapshot("sb1"));
        let err = rt.restore("sb1").unwrap_err();
        assert!(
            format!("{err:#}").contains("no complete checkpoint"),
            "{err:#}"
        );
    }

    /// Images from before sealing are judged by the backend.
    #[test]
    fn an_unsealed_image_is_judged_by_the_backend() {
        let dir = TempDir::new().unwrap();
        let (_, rt) = rec_rt(dir.path());
        let image = dir.path().join("snapshots/sb1");
        std::fs::create_dir_all(&image).unwrap();
        assert!(!rt.has_snapshot("sb1"));
        std::fs::write(image.join("checkpoint.img"), b"pages").unwrap();
        assert!(rt.has_snapshot("sb1"));
    }

    #[test]
    fn the_fingerprint_follows_the_backend() {
        let dir = TempDir::new().unwrap();
        let cfg = RuntimeConfig {
            work_dir: dir.path().to_path_buf(),
            volume_root: dir.path().join("volumes"),
            ..Default::default()
        };
        let rt = Runtime::with_backend(cfg, Arc::new(Recorder::default())).unwrap();
        assert_eq!(rt.fingerprint(), fingerprint("recorder 1"));
        assert_eq!(rt.host().engine, "recorder 1");
    }
}

#[cfg(test)]
mod observe_tests {
    use super::*;
    use tempfile::TempDir;

    /// An image the runsc backend's `validate` accepts: the statefile header,
    /// its metadata, and a byte past it.
    pub(super) fn fake_statefile(image: &Path) {
        std::fs::create_dir_all(image).unwrap();
        let meta = b"{}";
        let mut f = b"gVisorSF".to_vec();
        f.extend((meta.len() as u64).to_be_bytes());
        f.extend(meta);
        f.push(0);
        std::fs::write(image.join("checkpoint.img"), f).unwrap();
    }

    /// The directory a failed checkpoint used to leave behind.
    #[test]
    fn an_empty_image_directory_is_not_a_snapshot() {
        let td = TempDir::new().unwrap();
        std::fs::create_dir_all(td.path().join("bundles/sb1")).unwrap();
        std::fs::create_dir_all(td.path().join("snapshots/sb1")).unwrap();
        assert!(rt(td.path()).observe().unwrap().is_empty());
    }

    /// A split image without its pages, and a statefile cut short.
    #[test]
    fn runsc_rejects_an_incomplete_image() {
        let td = TempDir::new().unwrap();
        let image = td.path().join("img");
        fake_statefile(&image);
        let runsc = Runsc {
            bin: "runsc".into(),
            root: td.path().to_path_buf(),
            work_dir: td.path().to_path_buf(),
        };
        runsc.validate(&image).unwrap();
        std::fs::write(image.join("pages_meta.img"), b"m").unwrap();
        assert!(runsc.validate(&image).is_err());
        std::fs::write(image.join("pages.img"), b"").unwrap();
        runsc.validate(&image).unwrap();
        std::fs::write(
            image.join("checkpoint.img"),
            b"gVisorSF\0\0\0\0\0\0\0\x09{}",
        )
        .unwrap();
        assert!(runsc.validate(&image).is_err());
    }

    fn rt(work: &std::path::Path) -> Runtime {
        Runtime::new(RuntimeConfig {
            work_dir: work.to_path_buf(),
            volume_root: work.join("volumes"),
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn an_empty_node_observes_nothing() {
        let td = TempDir::new().unwrap();
        assert!(rt(td.path()).observe().unwrap().is_empty());
    }

    /// A bundle with nothing in its cgroup is a failed start, not a running
    /// sandbox. Reporting it as present would stop the master retrying it.
    #[test]
    fn a_bundle_without_processes_is_not_reported() {
        let td = TempDir::new().unwrap();
        std::fs::create_dir_all(td.path().join("bundles/sb1")).unwrap();
        assert!(rt(td.path()).observe().unwrap().is_empty());
    }

    /// Snapshot is read from the filesystem, so it needs no runsc and no
    /// cgroup -- which is what makes it observable at all.
    #[test]
    fn a_checkpointed_sandbox_is_observed_as_snapshot() {
        let td = TempDir::new().unwrap();
        std::fs::create_dir_all(td.path().join("bundles/sb1")).unwrap();
        fake_statefile(&td.path().join("snapshots/sb1"));
        let got = rt(td.path()).observe().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].tier, Tier::Snapshot);
        assert!(!got[0].serving);
    }

    /// A detached sandbox has no bundle, so it is not on this node at all.
    #[test]
    fn a_detached_sandbox_is_not_on_this_node() {
        let td = TempDir::new().unwrap();
        std::fs::create_dir_all(td.path().join("volumes/sb1/_archive")).unwrap();
        assert!(rt(td.path()).observe().unwrap().is_empty());
    }

    #[test]
    fn only_our_own_bundles_can_ever_be_seen() {
        let td = TempDir::new().unwrap();
        // Another deployment's bundle, in a sibling directory.
        std::fs::create_dir_all(td.path().join("../other/bundles/theirs")).ok();
        std::fs::create_dir_all(td.path().join("bundles/ours")).unwrap();
        fake_statefile(&td.path().join("snapshots/ours"));
        let got = rt(td.path()).observe().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "ours");
    }
}

/// Written into an image last, before it is published: the length of every
/// file the backend wrote. A dot name, so tag listings and transfers skip
/// it, as they skip the backend's own caches (`.mem.assembled`).
///
/// It catches a checkpoint that failed, not a power cut: nothing here is
/// fsynced, since syncing the whole image would put its every byte on the
/// snapshot tier's critical path.
const SEAL: &str = ".sealed";

/// The files of an image and their lengths, leaving out dot names.
fn image_files(image: &Path) -> Result<BTreeMap<String, u64>> {
    let mut files = BTreeMap::new();
    for e in std::fs::read_dir(image)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with('.') && e.file_type()?.is_file() {
            files.insert(name, e.metadata()?.len());
        }
    }
    Ok(files)
}

fn seal(image: &Path) -> Result<()> {
    std::fs::write(image.join(SEAL), serde_json::to_vec(&image_files(image)?)?)
        .context("sealing the checkpoint")
}

/// `.<name>.attempt-`: attempts at `dest` sit beside it under dot names,
/// on the same filesystem, so publishing one is a rename.
fn attempt_prefix(dest: &Path) -> String {
    format!(
        ".{}.attempt-",
        dest.file_name().unwrap_or_default().to_string_lossy()
    )
}

/// Remove earlier attempts at `dest`, returning the directory they were in.
fn clear_attempts(dest: &Path) -> Result<&Path> {
    let parent = dest.parent().context("an image path has a parent")?;
    std::fs::create_dir_all(parent)?;
    let prefix = attempt_prefix(dest);
    for e in std::fs::read_dir(parent)?.flatten() {
        if e.file_name().to_string_lossy().starts_with(&prefix) {
            std::fs::remove_dir_all(e.path()).ok();
        }
    }
    Ok(parent)
}

/// Rename `from` to `to` unless `to` exists. `renameat2(RENAME_NOREPLACE)`
/// makes that one step where the filesystem supports it; elsewhere the
/// check and the rename are two, which holds because one sandbox's
/// transitions never run at once.
fn publish(from: &Path, to: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let (f, t) = (
            std::ffi::CString::new(from.as_os_str().as_bytes())?,
            std::ffi::CString::new(to.as_os_str().as_bytes())?,
        );
        // SAFETY: both strings outlive the call, which keeps no pointer.
        let r = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                f.as_ptr(),
                libc::AT_FDCWD,
                t.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if r == 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINVAL) {
            return Err(e).with_context(|| format!("publishing {}", to.display()));
        }
    }
    if to.exists() {
        bail!("{} already exists", to.display());
    }
    std::fs::rename(from, to).with_context(|| format!("publishing {}", to.display()))
}

fn dir_size(p: &std::path::Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(p) else {
        return 0;
    };
    rd.flatten()
        .filter_map(|e| e.metadata().ok())
        .map(|m| if m.is_dir() { 0 } else { m.len() })
        .sum()
}

#[cfg(test)]
mod warm_tests {
    use super::*;

    fn rt(work: &Path) -> Runtime {
        Runtime::new(RuntimeConfig {
            work_dir: work.to_path_buf(),
            volume_root: work.join("volumes"),
            ..Default::default()
        })
        .unwrap()
    }

    /// A seed is the node's own business: reporting it would have the
    /// master destroy it as an orphan halfway through being imaged.
    #[test]
    fn a_seed_is_never_reported() {
        let dir = tempfile::tempdir().unwrap();
        let rt = rt(dir.path());
        for id in ["sb-real", "sandstorm-warm-abc123"] {
            std::fs::create_dir_all(dir.path().join("bundles").join(id)).unwrap();
            super::observe_tests::fake_statefile(&dir.path().join("snapshots").join(id));
        }
        let ids: Vec<String> = rt.observe().unwrap().into_iter().map(|o| o.id).collect();
        assert_eq!(ids, vec!["sb-real".to_string()]);
    }

    /// Anything that would make an image wrong makes a new key.
    #[test]
    fn the_warm_key_follows_the_spec_port_and_rootfs() {
        let dir = tempfile::tempdir().unwrap();
        let rt = rt(dir.path());
        let rootfs = dir.path().join("templates/py/rootfs");
        std::fs::create_dir_all(rootfs.join("app")).unwrap();
        let spec = serde_json::json!({"args": ["python3", "app.py"], "warm": {}});
        let a = rt.warm_dir("py", &spec, 8080, &rootfs);
        assert_eq!(a, rt.warm_dir("py", &spec, 8080, &rootfs), "stable");
        assert_ne!(a, rt.warm_dir("py", &spec, 9090, &rootfs), "port");
        let other = serde_json::json!({"args": ["python3", "other.py"], "warm": {}});
        assert_ne!(a, rt.warm_dir("py", &other, 8080, &rootfs), "spec");
        assert!(a.starts_with(dir.path().join("warm/py")));

        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(rootfs.join("app/new.py"), "").unwrap();
        assert_ne!(a, rt.warm_dir("py", &spec, 8080, &rootfs), "rootfs changed");
    }

    /// The sandbox's program runs on these files, and a warm image holds it
    /// laid out for the ones it saw.
    #[test]
    fn the_warm_key_follows_the_host_files_bound_in() {
        let dir = tempfile::tempdir().unwrap();
        let rt = rt(dir.path());
        let rootfs = dir.path().join("templates/py/rootfs");
        std::fs::create_dir_all(rootfs.join("app")).unwrap();
        let usr = dir.path().join("usr");
        std::fs::create_dir_all(usr.join("bin")).unwrap();
        let spec = serde_json::json!({
            "args": ["python3", "app.py"], "warm": {},
            "ro_binds": [usr.to_string_lossy()],
        });
        let a = rt.warm_dir("py", &spec, 8080, &rootfs);
        assert_eq!(a, rt.warm_dir("py", &spec, 8080, &rootfs), "stable");

        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(usr.join("bin/python3.new"), "").unwrap();
        std::fs::rename(usr.join("bin/python3.new"), usr.join("bin/python3")).unwrap();
        assert_ne!(a, rt.warm_dir("py", &spec, 8080, &rootfs), "upgraded");
    }

    /// The same, for one the key did not catch. A fresh image that dies is
    /// the program's doing, and is not to be seeded again straight away.
    #[test]
    fn an_image_nothing_survives_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let image = |name: &str, age: Duration| {
            let d = dir.path().join(name);
            std::fs::create_dir_all(&d).unwrap();
            let ready = std::fs::File::create(d.join("ready")).unwrap();
            ready
                .set_modified(std::time::SystemTime::now() - age)
                .unwrap();
            let at = std::fs::metadata(d.join("ready")).unwrap().modified().ok();
            (d, at)
        };
        let hour = Duration::from_secs(3600);

        let (old, ready) = image("old", hour);
        drop_warm_image("sb", &old, ready);
        assert!(!old.exists(), "dropped");
        assert!(!old.with_extension("failed").exists(), "and may be seeded");

        let (young, ready) = image("young", Duration::ZERO);
        drop_warm_image("sb", &young, ready);
        assert!(!young.exists(), "dropped");
        assert!(young.with_extension("failed").exists(), "not again yet");

        // A later sandbox dying of the image this one's death replaced.
        let (next, _) = image("next", Duration::ZERO);
        drop_warm_image("sb", &next, Some(std::time::UNIX_EPOCH));
        assert!(next.exists(), "the new image stays");
    }

    #[test]
    fn warm_is_opt_in_and_parses_with_defaults() {
        let plain: TemplateSpec =
            serde_json::from_value(serde_json::json!({"args": ["x"]})).unwrap();
        assert!(plain.warm.is_none());
        let warm: TemplateSpec = serde_json::from_value(
            serde_json::json!({"args": ["x"], "warm": {"after_restore": ["kill", "-USR1", "1"]}}),
        )
        .unwrap();
        let w = warm.warm.unwrap();
        assert_eq!(w.after_restore, vec!["kill", "-USR1", "1"]);
        assert_eq!(w.ready_timeout_secs, 60);
    }

    /// The node's slot and the master's plan must not overlap.
    #[test]
    fn the_reserved_slot_is_past_the_masters_plan() {
        let net = NetConfig::default();
        assert_eq!(net.prefix, 16);
        assert_eq!(net.reserved_index(), 16383);
        assert!(
            net.slot("seed", net.reserved_index()).is_ok(),
            "it is a real slot"
        );
        assert!(
            net.slot("x", net.reserved_index() + 1).is_err(),
            "and the last one"
        );
    }
}

#[cfg(test)]
#[path = "detach_tests.rs"]
mod detach_tests;
