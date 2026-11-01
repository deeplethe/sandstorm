//! A network namespace per sandbox.
//!
//! Sandboxes used to share the host's namespace (`--network=host`). Measured
//! on bm, that let code inside a sandbox connect to the master's API, to the
//! host's sshd, and to sibling sandboxes. Each sandbox now gets its own
//! namespace and its own address, reachable from the node and from nowhere
//! else.
//!
//! The arrangement, verified by hand before any of this was written:
//!
//! ```text
//!   node                         sandbox n
//!   sst<n>h  10.86.x.y+1/30  <-> sst<n>s  10.86.x.y+2/30   (its netns)
//! ```
//!
//! A /30 per sandbox rather than a shared bridge: with no layer-2 segment in
//! common there is nothing for sandboxes to talk over, so isolation does not
//! depend on getting bridge port isolation right.
//!
//! Three firewall rules do the rest, and they are explicit rather than
//! relying on `ip_forward=0`: that sysctl belongs to whoever installed
//! Docker, and this host's FORWARD policy is ACCEPT. Verified with
//! `ip_forward=1`: siblings, the host's sshd and the internet all stayed
//! unreachable while the node could still reach the sandbox.
//!
//! Egress is an exception made per sandbox, ahead of those rules; see
//! `EgressRules`. A sandbox with no exception costs no rule, so a create pays
//! nothing for the feature unless it uses it, and an exception that fails to
//! install leaves the sandbox as closed as it was.

use crate::proto::Egress;
use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::io::Write;
use std::net::Ipv4Addr;
use std::process::{Command as Exec, Stdio};

/// Address plan for one node.
#[derive(Debug, Clone)]
pub struct NetConfig {
    /// First address of the range carved into /30s. 10.86.0.0 by default,
    /// which gives 16384 sandboxes out of a /16.
    pub base: Ipv4Addr,
    /// Prefix of the whole range, used for the firewall rules.
    pub prefix: u8,
}

impl Default for NetConfig {
    fn default() -> Self {
        Self {
            base: Ipv4Addr::new(10, 86, 0, 0),
            prefix: 16,
        }
    }
}

/// One sandbox's slot in the plan.
/// A short, stable tag for a sandbox's veth names.
///
/// Named from the sandbox id, not its network index, because **both ends of
/// a veth pair are created in the host's namespace** before one is moved
/// into the sandbox's -- so both names have to be unique across the whole
/// machine. The index is allocated per node, so two nodes on one machine
/// both start at 0 and collided: `Cannot find device "sst0s"`, which backoff
/// and retry papered over. A hash rather than a slice of the id because a
/// caller may supply an id that is not hex.
///
/// 10 hex characters keeps `sst<tag>h` at 14 bytes, inside IFNAMSIZ's 15.
fn if_tag(id: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(id.as_bytes()))[..10].to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    pub index: u32,
    /// The node's end of the veth.
    pub host_ip: Ipv4Addr,
    /// The sandbox's address, which is how anything reaches it.
    pub sandbox_ip: Ipv4Addr,
    /// Named after the sandbox, not its slot: after a restart the node has
    /// an id and must find the namespace from it.
    pub netns: String,
    host_if: String,
    sandbox_if: String,
}

impl NetConfig {
    pub fn subnet(&self) -> String {
        format!("{}/{}", self.base, self.prefix)
    }

    /// The one slot the master never hands out, kept for the node's own
    /// use: the seed sandbox a warm template image is taken from. The
    /// master's plan stops one short of it (`max_net_index` in main.rs).
    pub fn reserved_index(&self) -> u32 {
        (1u32 << (32 - self.prefix.min(30) as u32)) / 4 - 1
    }

    /// The /30 for one index. Interface names stay inside the kernel's
    /// 15-character limit even at the top of the range.
    pub fn slot(&self, id: &str, index: u32) -> Result<Slot> {
        let span = 1u32 << (32 - self.prefix as u32);
        let offset = index
            .checked_mul(4)
            .filter(|o| o + 4 <= span)
            .with_context(|| {
                format!(
                    "index {index} does not fit in {} -- the node is out of addresses",
                    self.subnet()
                )
            })?;
        let base: u32 = self.base.into();
        Ok(Slot {
            index,
            host_ip: Ipv4Addr::from(base + offset + 1),
            sandbox_ip: Ipv4Addr::from(base + offset + 2),
            netns: format!("sandstorm-{id}"),
            host_if: format!("sst{}h", if_tag(id)),
            sandbox_if: format!("sst{}s", if_tag(id)),
        })
    }
}

/// A command to run. `iptables` waits for its lock rather than failing:
/// sandboxes start side by side, and on the legacy backend a second one at
/// the same moment exits 4 without looking.
fn command(args: &[&str]) -> Exec {
    let mut cmd = Exec::new(args[0]);
    if args[0] == "iptables" {
        cmd.arg("-w");
    }
    cmd.args(&args[1..]);
    cmd
}

fn run(what: &str, args: &[&str]) -> Result<()> {
    let out = command(args)
        .output()
        .with_context(|| format!("running {:?}", args))?;
    if !out.status.success() {
        bail!(
            "{what}: {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn ok(args: &[&str]) -> bool {
    command(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Build the namespace and veth for one sandbox. Idempotent: an existing
/// namespace is reused, which is what makes re-adoption after a restart free.
/// The interfaces in `ip -o -4 addr show` output that are a sandbox's host
/// end (`sst...h`) and not `ours`: veths left behind holding an address.
fn stale_holders(ip_output: &str, ours: &str) -> Vec<String> {
    ip_output
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .map(|name| name.split('@').next().unwrap_or(name))
        .filter(|name| name.starts_with("sst") && name.ends_with('h') && *name != ours)
        .map(str::to_string)
        .collect()
}

pub fn setup(cfg: &NetConfig, id: &str, index: u32, pool: Option<&Pool>) -> Result<Slot> {
    let (s, pending) = setup_started(cfg, id, index, pool)?;
    pending.wait()?;
    Ok(s)
}

/// `setup`, returning before a pooled namespace's veth is named and
/// addressed; see `Pending`. Built from nothing, it is all done by return.
pub fn setup_started(
    cfg: &NetConfig,
    id: &str,
    index: u32,
    pool: Option<&Pool>,
) -> Result<(Slot, Pending)> {
    let s = named(cfg.slot(id, index)?);

    if !std::path::Path::new(&format!("{NETNS_DIR}/{}", s.netns)).exists() {
        // A namespace built ahead of time, if there is one: what is left is
        // naming and addressing it. Anything that goes wrong on the way
        // falls through to building it here, as without a pool.
        if let Some(blank) = pool.and_then(Pool::take) {
            let t0 = std::time::Instant::now();
            let claimed = Slot {
                host_if: blank.host_if.clone(),
                sandbox_if: blank.sandbox_if.clone(),
                ..s.clone()
            };
            match claim(&blank, &claimed) {
                Ok(pending) => {
                    tracing::debug!(%id, ms = t0.elapsed().as_millis() as u64, "claimed a pooled namespace");
                    return Ok((claimed, pending));
                }
                Err(e) => {
                    tracing::warn!(%id, error = %e, "could not use a pooled namespace; building one");
                    let _ = run("removing netns", &["ip", "netns", "del", &s.netns]);
                    let _ = run("removing netns", &["ip", "netns", "del", &blank.netns]);
                    let _ = run("removing veth", &["ip", "link", "del", &blank.host_if]);
                    let _ = std::fs::remove_file(format!("{VETH_NAMES}/{}", s.netns));
                }
            }
        }
        run("creating netns", &["ip", "netns", "add", &s.netns])?;
    }
    if !ok(&["ip", "link", "show", &s.host_if]) {
        run(
            "creating veth",
            &[
                "ip",
                "link",
                "add",
                &s.host_if,
                "type",
                "veth",
                "peer",
                "name",
                &s.sandbox_if,
            ],
        )?;
        run(
            "moving the peer into the netns",
            &["ip", "link", "set", &s.sandbox_if, "netns", &s.netns],
        )?;
    }

    let host_cidr = format!("{}/30", s.host_ip);
    let sb_cidr = format!("{}/30", s.sandbox_ip);
    let gw = s.host_ip.to_string();
    // An address is one slot's, but a veth left behind by a sandbox that
    // died outlives the slot being handed to someone else. Found on f9: two
    // host interfaces with 10.86.0.253/30, the route to .254 through the
    // dead one, and the new sandbox, running, unreachable for good.
    let held = Exec::new("ip")
        .args([
            "-o",
            "-4",
            "addr",
            "show",
            "to",
            &format!("{}/32", s.host_ip),
        ])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    for stale in stale_holders(&held, &s.host_if) {
        tracing::warn!(%id, interface = %stale, ip = %s.host_ip, "removing a stale interface holding this slot's address");
        let _ = run("removing a stale veth", &["ip", "link", "del", &stale]);
    }
    // Re-running these on an existing slot is an error we ignore: the point
    // is the end state, not whether this call is the one that got there.
    let _ = run(
        "addr",
        &["ip", "addr", "add", &host_cidr, "dev", &s.host_if],
    );
    run("host link up", &["ip", "link", "set", &s.host_if, "up"])?;
    let _ = run(
        "sandbox addr",
        &[
            "ip",
            "-n",
            &s.netns,
            "addr",
            "add",
            &sb_cidr,
            "dev",
            &s.sandbox_if,
        ],
    );
    run(
        "sandbox link up",
        &["ip", "-n", &s.netns, "link", "set", &s.sandbox_if, "up"],
    )?;
    run(
        "loopback up",
        &["ip", "-n", &s.netns, "link", "set", "lo", "up"],
    )?;
    let _ = run(
        "default route",
        &["ip", "-n", &s.netns, "route", "add", "default", "via", &gw],
    );
    Ok((s, Pending::done()))
}

const NETNS_DIR: &str = "/var/run/netns";

/// Which veth a namespace from the pool has, keyed by the namespace's name:
/// "<host end> <sandbox end>". A claimed namespace keeps its pooled
/// interfaces' names rather than taking ones derived from the sandbox:
/// renaming an interface took 6-11 ms on bare metal, holding the RTNL lock
/// the restore beside it needs, and a claim renamed two. Everything that
/// reaches a sandbox's veth by name reads them from here first.
const VETH_NAMES: &str = "/run/sandstorm/veth";

/// `s` with the veth names on record for its namespace, if there are any.
fn named(mut s: Slot) -> Slot {
    if let Ok(names) = std::fs::read_to_string(format!("{VETH_NAMES}/{}", s.netns)) {
        let mut it = names.split_whitespace();
        if let (Some(h), Some(b)) = (it.next(), it.next()) {
            s.host_if = h.to_string();
            s.sandbox_if = b.to_string();
        }
    }
    s
}

fn record_names(s: &Slot) -> Result<()> {
    std::fs::create_dir_all(VETH_NAMES)?;
    std::fs::write(
        format!("{VETH_NAMES}/{}", s.netns),
        format!("{} {}\n", s.host_if, s.sandbox_if),
    )
    .context("recording the veth's names")
}

/// The backend's own setup inside a pooled namespace (`Backend::prepare_netns`).
pub type Prepare = Box<dyn Fn(&str) -> Result<()> + Send + Sync>;

/// How long the pool waits after a namespace is taken before building the
/// next. Building one takes the kernel's RTNL lock, and so does every step of
/// a claim: refilling straight away put a claim behind it, 27-37 ms on bm
/// against a few with the lock to itself. The sandbox that took one is
/// usually still being restored, and a burst takes several in a row.
const REFILL_QUIET: std::time::Duration = std::time::Duration::from_millis(250);

/// Retired namespaces past which they are deleted without waiting for quiet.
const MAX_RETIRED: usize = 64;

/// How often a refill waiting for quiet looks again at how low the pool is.
const REFILL_RECHECK: std::time::Duration = std::time::Duration::from_millis(10);

/// Namespaces built before any sandbox asks for one.
///
/// Building a namespace is about a dozen `ip` processes, and for a
/// Firecracker sandbox another nine for its tap and NAT rules, all on the
/// path of a create or a wake from Snapshot. Measured on bm with warm
/// templates: CubeSandbox, which keeps tap devices ready, answered in 55 ms
/// against 161 ms here, and only 70-76 ms of ours was the restore itself.
///
/// None of that work depends on the sandbox. Its address comes from the
/// master's index and its names from its id, so a pooled namespace is built
/// with neither -- the namespace, the veth with its peer inside, loopback
/// up, and whatever the backend puts in it -- and `claim` renames and
/// addresses it when a sandbox takes it. An empty pool is no error: `setup`
/// builds as it always did.
pub struct Pool {
    target: usize,
    /// Keeps two nodes on one machine out of each other's pool, the same
    /// problem `if_tag` solves for sandboxes.
    tag: String,
    prepare: Prepare,
    state: std::sync::Mutex<PoolState>,
    wanted: std::sync::Condvar,
}

#[derive(Default)]
struct PoolState {
    ready: Vec<Blank>,
    next: u64,
    last_take: Option<std::time::Instant>,
    /// Namespaces of destroyed sandboxes, renamed out of the way: made
    /// ready again while the pool is short, deleted when nothing is being
    /// created (`Pool::retire`).
    retired: Vec<Blank>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blank {
    netns: String,
    host_if: String,
    sandbox_if: String,
}

impl Pool {
    /// Start keeping `target` namespaces ready. `key` tells this node's
    /// pool from another's on the same machine (its work directory);
    /// `prepare` is the backend's own setup inside a namespace.
    ///
    /// Whatever a previous run of this node left in its pool is removed
    /// first: a pooled namespace holds nothing of a sandbox's.
    pub fn start(target: usize, key: &str, prepare: Prepare) -> std::sync::Arc<Self> {
        // Numbering starts somewhere new each run. Deleting a namespace
        // returns before the kernel has destroyed the veth in it, so the
        // names a previous run used can still be taken a moment later:
        // seen on bm, the first build after a restart failed with "File
        // exists" on the old run's first name.
        let next = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or_default();
        let pool = std::sync::Arc::new(Self {
            target,
            tag: if_tag(key)[..6].to_string(),
            prepare,
            state: std::sync::Mutex::new(PoolState {
                ready: Vec::new(),
                next,
                last_take: None,
                retired: Vec::new(),
            }),
            wanted: Default::default(),
        });
        let (prefix, dead) = (pool.netns_prefix(), pool.dead_prefix());
        if let Ok(rd) = std::fs::read_dir(NETNS_DIR) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with(&prefix) || name.starts_with(&dead) {
                    // The peer goes with the namespace, and the host end
                    // with its peer.
                    let _ = run("removing a pooled netns", &["ip", "netns", "del", &name]);
                }
            }
        }
        let p = pool.clone();
        std::thread::Builder::new()
            .name("netns-pool".into())
            .spawn(move || p.fill())
            .expect("spawning the netns pool");
        pool
    }

    fn netns_prefix(&self) -> String {
        format!("sstpool-{}-", self.tag)
    }

    fn dead_prefix(&self) -> String {
        format!("sstdead-{}-", self.tag)
    }

    /// Take a destroyed sandbox's namespace off its hands, to be deleted
    /// when nothing is being created.
    ///
    /// Deleting a namespace holds the kernel's networking locks for a long
    /// while, and a create right behind a delete waited on them: on bare
    /// metal a warm start one after another took 104 ms where one on its own
    /// took 70. The caller has already brought the host end down, so the
    /// slot's route is gone and its address free for the next sandbox; the
    /// namespace is renamed here so the sandbox's own name is free too -- a
    /// sandbox put on disk and woken comes back under it.
    pub fn retire(&self, netns: &str, host_if: &str, sandbox_if: &str) -> Result<()> {
        let n = {
            let mut st = self.state.lock().unwrap();
            st.next += 1;
            st.next & 0xff_ffff
        };
        let dead = format!("{}{n}", self.dead_prefix());
        rename_netns(netns, &dead)?;
        self.state.lock().unwrap().retired.push(Blank {
            netns: dead,
            host_if: host_if.to_string(),
            sandbox_if: sandbox_if.to_string(),
        });
        self.wanted.notify_one();
        Ok(())
    }

    /// Names for the `n`th namespace this pool builds. The interface names
    /// have to be unique on the machine, like a sandbox's, and fit
    /// IFNAMSIZ: `sp` + 6 + 6 + 1 is 15.
    fn blank(&self, n: u64) -> Blank {
        let n = n & 0xff_ffff;
        Blank {
            netns: format!("{}{n}", self.netns_prefix()),
            host_if: format!("sp{}{n:06x}h", self.tag),
            sandbox_if: format!("sp{}{n:06x}s", self.tag),
        }
    }

    /// One namespace, if one is ready, and a nudge to build its successor.
    pub fn take(&self) -> Option<Blank> {
        let b = {
            let mut st = self.state.lock().unwrap();
            st.last_take = Some(std::time::Instant::now());
            st.ready.pop()
        };
        self.wanted.notify_one();
        b
    }

    /// How much longer to stay out of the way of a claim; see `REFILL_QUIET`.
    fn quiet_left(&self) -> Option<std::time::Duration> {
        let since = self.state.lock().unwrap().last_take?.elapsed();
        REFILL_QUIET.checked_sub(since).filter(|d| !d.is_zero())
    }

    /// Whether the pool must build or delete now, quiet or not: destroys
    /// have outrun it, or it is nearly empty. Creates one after another
    /// never leave a quiet spell, and on bare metal a pool of 32 ran dry
    /// halfway through 42 back-to-back starts, the rest building their own
    /// namespace at about 100 ms more each.
    /// Also when there is a namespace to reuse, which needs no quiet.
    fn pressed(&self) -> (bool, bool) {
        let st = self.state.lock().unwrap();
        let low = st.ready.len() < (self.target / 4).max(2);
        let reuse = st.ready.len() < self.target && !st.retired.is_empty();
        (low, low || reuse || st.retired.len() > MAX_RETIRED)
    }

    fn reusable(&self) -> bool {
        let st = self.state.lock().unwrap();
        st.ready.len() < self.target && !st.retired.is_empty()
    }

    fn fill(&self) {
        loop {
            let reuse = {
                let mut st = self.state.lock().unwrap();
                while st.ready.len() >= self.target && st.retired.is_empty() {
                    st = self.wanted.wait(st).unwrap();
                }
                if st.ready.len() < self.target {
                    st.retired.pop()
                } else {
                    None
                }
            };
            // A destroyed sandbox's namespace before a new one, and without
            // waiting for quiet: it has everything a new one would be built
            // with, and making it ready again takes none of what holds the
            // kernel's networking locks for long. Building under load put
            // about 10 ms on every warm start on bm.
            if let Some(dead) = reuse {
                match self.recycle(&dead) {
                    Ok(b) => self.state.lock().unwrap().ready.push(b),
                    Err(e) => {
                        tracing::debug!(netns = %dead.netns, error = %e, "could not reuse a namespace; deleting it");
                        remove(&dead);
                    }
                }
                continue;
            }
            // Out of the way of creates, unless pressed. Asked again on
            // every pass: a wait begun with the pool full is extended by
            // every take, and once ran on until the pool was empty -- the
            // 33rd of a run of back-to-back starts took 185 ms, not 70.
            let low = loop {
                let (low, pressed) = self.pressed();
                match self.quiet_left() {
                    Some(wait) if !pressed => std::thread::sleep(wait.min(REFILL_RECHECK)),
                    _ => break low,
                }
            };
            if self.reusable() {
                continue;
            }
            let dead = if low {
                None
            } else {
                self.state.lock().unwrap().retired.pop()
            };
            if let Some(dead) = dead {
                remove(&dead);
                continue;
            }
            let n = {
                let mut st = self.state.lock().unwrap();
                if st.ready.len() >= self.target {
                    continue;
                }
                st.next += 1;
                st.next
            };
            let b = self.blank(n);
            match self.build(&b) {
                Ok(()) => self.state.lock().unwrap().ready.push(b),
                Err(e) => {
                    tracing::warn!(netns = %b.netns, error = %e, "could not build a pooled namespace");
                    let _ = run("removing netns", &["ip", "netns", "del", &b.netns]);
                    std::thread::sleep(std::time::Duration::from_secs(5));
                }
            }
        }
    }

    /// A retired namespace made a blank again. What its sandbox left in it
    /// is its end's address and routes, which the next claim replaces, and
    /// connections the kernel still tracks: emptied, so that a late answer
    /// to one is not translated to the next sandbox's guest. The host end is
    /// already down (`teardown`).
    fn recycle(&self, dead: &Blank) -> Result<Blank> {
        let path = format!("{NETNS_DIR}/{}", dead.netns);
        let sandbox_if = dead.sandbox_if.clone();
        std::thread::spawn(move || -> Result<()> {
            ifconf::enter(&path)?;
            ifconf::scrub(&sandbox_if)
        })
        .join()
        .map_err(|_| anyhow::anyhow!("scrubbing {} panicked", dead.netns))??;
        let n = {
            let mut st = self.state.lock().unwrap();
            st.next += 1;
            st.next & 0xff_ffff
        };
        let netns = format!("{}{n}", self.netns_prefix());
        rename_netns(&dead.netns, &netns)?;
        Ok(Blank {
            netns,
            ..dead.clone()
        })
    }

    fn build(&self, b: &Blank) -> Result<()> {
        run("creating netns", &["ip", "netns", "add", &b.netns])?;
        // Both ends are created in the host's namespace and the peer moved
        // in; `ip` does that in one call.
        run(
            "creating veth",
            &[
                "ip",
                "link",
                "add",
                &b.host_if,
                "type",
                "veth",
                "peer",
                "name",
                &b.sandbox_if,
                "netns",
                &b.netns,
            ],
        )?;
        run(
            "loopback up",
            &["ip", "-n", &b.netns, "link", "set", "lo", "up"],
        )?;
        (self.prepare)(&b.netns)
    }
}

/// Delete a namespace, which takes the peer with it, and the host end.
fn remove(b: &Blank) {
    let _ = run("removing netns", &["ip", "netns", "del", &b.netns]);
    let _ = run("removing veth", &["ip", "link", "del", &b.host_if]);
}

/// Turn a pooled namespace into this sandbox's: the same names and
/// addresses `setup` would have built, so nothing after it can tell.
///
/// In-process, not `ip`: a process from `sandstormd` cost 5-10 ms on bm, and
/// this was three of them, 19 ms of a 64 ms warm start. The host's end is
/// configured from this thread; the sandbox's from a thread that has entered
/// its namespace, since the ioctls act on the caller's.
fn claim(b: &Blank, s: &Slot) -> Result<Pending> {
    // Named now: everything after this finds the namespace by the
    // sandbox's name, Firecracker entering it first of all.
    rename_netns(&b.netns, &s.netns)?;
    record_names(s)?;
    let (b, s) = (b.clone(), s.clone());
    Ok(Pending(Some(std::thread::spawn(move || {
        // A veth left behind by a dead sandbox may hold this slot's /30;
        // see `setup`. Read from the routing table rather than from every
        // interface's addresses: bm had 514 interfaces, and listing them
        // was 3 ms of the claim. The harm a stale veth does is its route.
        let routes = std::fs::read_to_string("/proc/net/route").unwrap_or_default();
        for stale in route_holders(&routes, s.host_ip, &s.host_if) {
            tracing::warn!(interface = %stale, ip = %s.host_ip, "removing a stale interface holding this slot's address");
            let _ = run("removing a stale veth", &["ip", "link", "del", &stale]);
        }
        ifconf::configure(&b.host_if, &s.host_if, s.host_ip, None)
            .context("naming and addressing the host's end")?;
        // The ioctls act on the caller's namespace, so this thread enters
        // the sandbox's for its end, and is spent doing so.
        ifconf::enter(&format!("{NETNS_DIR}/{}", s.netns))?;
        ifconf::configure(&b.sandbox_if, &s.sandbox_if, s.sandbox_ip, Some(s.host_ip))
            .context("naming and addressing the sandbox's end")
    }))))
}

/// The rest of a claim, under way on its own thread: naming and addressing
/// the veth. Nothing on the sandbox's side of the namespace needs it -- a
/// Firecracker guest is on the tap, which the pool built -- only what
/// reaches the sandbox from the node does. So a create restores the guest
/// meanwhile and waits for this before it lets connections through. On bare
/// metal the ioctls took 25-35 ms, most of a warm start's 85.
pub struct Pending(Option<std::thread::JoinHandle<Result<()>>>);

impl Pending {
    pub fn done() -> Self {
        Self(None)
    }

    #[cfg(test)]
    pub fn failed() -> Self {
        Self(Some(std::thread::spawn(|| {
            bail!("the veth has gone");
        })))
    }

    pub fn wait(mut self) -> Result<()> {
        match self.0.take() {
            Some(h) => h
                .join()
                .map_err(|_| anyhow::anyhow!("the thread naming the sandbox's veth panicked"))?,
            None => Ok(()),
        }
    }
}

impl Drop for Pending {
    /// Never left running: whoever drops one unwaited still gets the
    /// interfaces in a known state before anything else touches them.
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            let _ = h.join();
        }
    }
}

/// Sandboxes' host ends other than `ours` that the kernel routes this slot's
/// /30 through, from `/proc/net/route`: the interface, then the destination
/// and, seven columns on, the mask, both as little-endian hex.
fn route_holders(table: &str, host_ip: Ipv4Addr, ours: &str) -> Vec<String> {
    let net = u32::from(host_ip) & !3;
    let addr =
        |h: &str| u32::from_str_radix(h, 16).map(|v| u32::from(Ipv4Addr::from(v.to_le_bytes())));
    table
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let (name, dest, mask) = (*f.first()?, *f.get(1)?, *f.get(7)?);
            (addr(dest).ok()? == net
                && addr(mask).ok()? == 0xffff_fffc
                && (name.starts_with("sst") || name.starts_with("sp"))
                && name.ends_with('h')
                && name != ours)
                .then(|| name.to_string())
        })
        .collect()
}

/// Interface configuration by ioctl, the calls `ifconfig` and `route` make.
/// Every slot is a /30 with its gateway on the other end, which is all this
/// covers.
#[cfg(target_os = "linux")]
mod ifconf {
    use anyhow::{bail, Context, Result};
    use std::net::Ipv4Addr;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    fn socket() -> Result<OwnedFd> {
        // SAFETY: plain socket(2); the result is checked before it is owned.
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("opening a socket");
        }
        // SAFETY: a fresh descriptor nothing else owns.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    pub(super) fn ifreq(name: &str) -> Result<libc::ifreq> {
        // SAFETY: an all-zero ifreq is a valid, empty request.
        let mut r: libc::ifreq = unsafe { std::mem::zeroed() };
        put_name(&mut r.ifr_name, name)?;
        Ok(r)
    }

    fn put_name(dst: &mut [libc::c_char; libc::IFNAMSIZ], name: &str) -> Result<()> {
        if name.len() >= libc::IFNAMSIZ || name.contains('\0') {
            bail!("{name:?} is not an interface name");
        }
        for (d, c) in dst.iter_mut().zip(name.bytes()) {
            *d = c as libc::c_char;
        }
        Ok(())
    }

    pub(super) fn sockaddr(ip: Ipv4Addr) -> libc::sockaddr {
        let sin = libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: 0,
            sin_addr: libc::in_addr {
                s_addr: u32::from(ip).to_be(),
            },
            sin_zero: [0; 8],
        };
        // SAFETY: sockaddr_in and sockaddr are both 16 bytes, and an AF_INET
        // sockaddr is read as a sockaddr_in by the kernel.
        unsafe { std::mem::transmute(sin) }
    }

    fn ioctl<T>(fd: &OwnedFd, req: libc::c_ulong, arg: &mut T, what: &str) -> Result<()> {
        // SAFETY: `arg` is the struct this request takes, and lives across
        // the call.
        if unsafe { libc::ioctl(fd.as_raw_fd(), req as _, arg as *mut T) } < 0 {
            return Err(std::io::Error::last_os_error()).context(what.to_string());
        }
        Ok(())
    }

    /// Rename `from` to `to` (it must still be down) unless they are the
    /// same, give it `ip`/30, bring it up, and route everything else through
    /// `gateway` if there is one.
    pub(super) fn configure(
        from: &str,
        to: &str,
        ip: Ipv4Addr,
        gateway: Option<Ipv4Addr>,
    ) -> Result<()> {
        let fd = socket()?;
        if from != to {
            let mut r = ifreq(from)?;
            put_name(unsafe { &mut r.ifr_ifru.ifru_newname }, to)?;
            ioctl(
                &fd,
                libc::SIOCSIFNAME as _,
                &mut r,
                &format!("renaming {from} to {to}"),
            )?;
        }

        let mut r = ifreq(to)?;
        r.ifr_ifru.ifru_addr = sockaddr(ip);
        ioctl(&fd, libc::SIOCSIFADDR as _, &mut r, "setting the address")?;
        let mut r = ifreq(to)?;
        r.ifr_ifru.ifru_netmask = sockaddr(Ipv4Addr::new(255, 255, 255, 252));
        ioctl(
            &fd,
            libc::SIOCSIFNETMASK as _,
            &mut r,
            "setting the netmask",
        )?;

        let mut r = ifreq(to)?;
        ioctl(&fd, libc::SIOCGIFFLAGS as _, &mut r, "reading the flags")?;
        // SAFETY: SIOCGIFFLAGS filled the flags member.
        unsafe { r.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short };
        ioctl(&fd, libc::SIOCSIFFLAGS as _, &mut r, "bringing it up")?;

        if let Some(gw) = gateway {
            // SAFETY: an all-zero rtentry is a valid, empty route.
            let mut rt: libc::rtentry = unsafe { std::mem::zeroed() };
            rt.rt_dst = sockaddr(Ipv4Addr::UNSPECIFIED);
            rt.rt_genmask = sockaddr(Ipv4Addr::UNSPECIFIED);
            rt.rt_gateway = sockaddr(gw);
            rt.rt_flags = libc::RTF_UP | libc::RTF_GATEWAY;
            ioctl(
                &fd,
                libc::SIOCADDRT as _,
                &mut rt,
                "adding the default route",
            )?;
        }
        Ok(())
    }

    /// Bring an interface down, which takes its connected route with it.
    pub(super) fn down(name: &str) -> Result<()> {
        let fd = socket()?;
        let mut r = ifreq(name)?;
        ioctl(&fd, libc::SIOCGIFFLAGS as _, &mut r, "reading the flags")?;
        // SAFETY: SIOCGIFFLAGS filled the flags member.
        unsafe { r.ifr_ifru.ifru_flags &= !(libc::IFF_UP as libc::c_short) };
        ioctl(&fd, libc::SIOCSIFFLAGS as _, &mut r, "bringing it down")
    }

    /// Ready the calling thread's namespace for another sandbox: its end
    /// down, which takes its routes, and every tracked connection dropped.
    pub(super) fn scrub(sandbox_if: &str) -> Result<()> {
        down(sandbox_if)?;
        flush_conntrack()
    }

    /// conntrack's table for this namespace emptied, as `conntrack -F`
    /// does: an IPCTNL_MSG_CT_DELETE that names no connection.
    fn flush_conntrack() -> Result<()> {
        // SAFETY: plain socket(2); the result is checked before it is owned.
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                libc::NETLINK_NETFILTER,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("opening a netfilter socket");
        }
        // SAFETY: a fresh descriptor nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        const NFNL_SUBSYS_CTNETLINK: u16 = 1;
        const IPCTNL_MSG_CT_DELETE: u16 = 2;
        let mut req = [0u8; 20];
        req[0..4].copy_from_slice(&20u32.to_ne_bytes());
        req[4..6]
            .copy_from_slice(&(NFNL_SUBSYS_CTNETLINK << 8 | IPCTNL_MSG_CT_DELETE).to_ne_bytes());
        req[6..8].copy_from_slice(&((libc::NLM_F_REQUEST | libc::NLM_F_ACK) as u16).to_ne_bytes());
        req[8..12].copy_from_slice(&1u32.to_ne_bytes());
        // nfgenmsg: AF_UNSPEC, every family; version 0; resource 0.
        // SAFETY: a buffer of its stated length; to the kernel, pid 0.
        if unsafe { libc::send(fd.as_raw_fd(), req.as_ptr().cast(), req.len(), 0) } < 0 {
            return Err(std::io::Error::last_os_error()).context("asking for a conntrack flush");
        }
        let mut buf = [0u8; 512];
        // SAFETY: reading into a buffer of its stated length.
        let n = unsafe { libc::recv(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n < 0 {
            return Err(std::io::Error::last_os_error())
                .context("reading the conntrack flush's answer");
        }
        // An nlmsghdr of NLMSG_ERROR, then the error: 0 is the ack.
        let n = n as usize;
        let kind = (n >= 20).then(|| u16::from_ne_bytes([buf[4], buf[5]]));
        if kind != Some(libc::NLMSG_ERROR as u16) {
            bail!("the kernel answered a conntrack flush with {kind:?}");
        }
        let err = i32::from_ne_bytes([buf[16], buf[17], buf[18], buf[19]]);
        if err != 0 {
            return Err(std::io::Error::from_raw_os_error(-err)).context("flushing conntrack");
        }
        Ok(())
    }

    /// Move the calling thread into the namespace at `path`. For a thread of
    /// its own: there is no going back.
    pub(super) fn enter(path: &str) -> Result<()> {
        let f = std::fs::File::open(path).with_context(|| format!("opening {path}"))?;
        // SAFETY: a namespace file descriptor this thread holds open.
        if unsafe { libc::setns(f.as_raw_fd(), libc::CLONE_NEWNET) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("entering {path}"));
        }
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
mod ifconf {
    use anyhow::{bail, Result};
    use std::net::Ipv4Addr;

    pub(super) fn configure(_: &str, _: &str, _: Ipv4Addr, _: Option<Ipv4Addr>) -> Result<()> {
        bail!("network namespaces are Linux only")
    }
    pub(super) fn scrub(_: &str) -> Result<()> {
        bail!("network namespaces are Linux only")
    }
    pub(super) fn enter(_: &str) -> Result<()> {
        bail!("network namespaces are Linux only")
    }
    pub(super) fn down(_: &str) -> Result<()> {
        bail!("network namespaces are Linux only")
    }
}

/// Give a named namespace another name. A name is a bind mount of the
/// namespace under /var/run/netns, which `rename` cannot move, so this is
/// what `ip netns attach` does: bind it under the new name, then drop the
/// old one.
#[cfg(target_os = "linux")]
fn rename_netns(from: &str, to: &str) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let dir = std::path::Path::new(NETNS_DIR);
    let (src, dst) = (dir.join(from), dir.join(to));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&dst)
        .with_context(|| format!("creating {}", dst.display()))?;
    let c = |p: &std::path::Path| CString::new(p.as_os_str().as_bytes()).expect("no NUL in a path");
    let (s, d) = (c(&src), c(&dst));
    // SAFETY: both are valid NUL-terminated paths; a bind mount takes no
    // filesystem type or data.
    let r = unsafe {
        libc::mount(
            s.as_ptr(),
            d.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        )
    };
    if r != 0 {
        let e = std::io::Error::last_os_error();
        let _ = std::fs::remove_file(&dst);
        bail!("binding {from} as {to}: {e}");
    }
    // SAFETY: a valid path; MNT_DETACH cannot fail on a busy mount.
    unsafe { libc::umount2(s.as_ptr(), libc::MNT_DETACH) };
    let _ = std::fs::remove_file(&src);
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn rename_netns(_from: &str, _to: &str) -> Result<()> {
    bail!("network namespaces are Linux only")
}

/// Take down a sandbox's namespace and veth while keeping its slot.
///
/// For the Snapshot tier: the process is gone and the image is on disk, so
/// nothing needs the kernel objects until a restore, which rebuilds them
/// with `setup` -- same index, same addresses, same names. They are not
/// small. Measured on the load-test fleet at ~0.4 MiB of unreclaimable
/// slab per namespace: 1538 snapshotted sandboxes held 694 MiB on a 1.9 GiB
/// node, which is what capped the cluster at 7284 sandboxes, not memory
/// for the sandboxes and not disk. The slot itself is a number in the
/// master's plan and costs nothing to keep.
/// Bring a sandbox's host end down, which frees its address for the next
/// sandbox at once: the route goes with it. For a destroy that finishes
/// later, off the node's command loop -- the master hands the address on as
/// soon as it has sent the destroy, and a second up interface on the same
/// /30 took the new sandbox's packets for a second.
pub fn quiesce(cfg: &NetConfig, id: &str, index: u32) -> Result<()> {
    let s = named(cfg.slot(id, index)?);
    if !has_namespace(&s) {
        return Ok(());
    }
    ifconf::down(&s.host_if)
}

/// Whether this sandbox still has its namespace, and so its veth.
///
/// The master asks for a destroy until a report no longer lists the sandbox,
/// so a repeat can arrive after the first is done: 52 of 943 on bare metal in
/// a run of bursts. By then a veth built for it under its own name is in the
/// pool, or is another sandbox's. Going by the name alone took one down or
/// deleted it, and the sandbox holding it could not be reached again.
fn has_namespace(s: &Slot) -> bool {
    std::path::Path::new(&format!("{NETNS_DIR}/{}", s.netns)).exists()
}

pub fn release(cfg: &NetConfig, id: &str, index: u32, pool: Option<&Pool>) -> Result<()> {
    teardown(cfg, id, index, pool)
}

pub fn teardown(cfg: &NetConfig, id: &str, index: u32, pool: Option<&Pool>) -> Result<()> {
    let s = named(cfg.slot(id, index)?);
    let _ = std::fs::remove_file(format!("{VETH_NAMES}/{}", s.netns));
    if !has_namespace(&s) {
        return Ok(());
    }
    // With a pool, the slot is freed now and the namespace deleted later;
    // see `Pool::retire`. Down first, so its route goes at once.
    if let Some(p) = pool {
        if ifconf::down(&s.host_if).is_ok() && p.retire(&s.netns, &s.host_if, &s.sandbox_if).is_ok()
        {
            return Ok(());
        }
    }
    // Deleting the netns takes the peer with it; the host end has to go too.
    let _ = run("removing netns", &["ip", "netns", "del", &s.netns]);
    let _ = run("removing veth", &["ip", "link", "del", &s.host_if]);
    Ok(())
}

/// Chain holding one REJECT per sandbox that is not currently hot.
///
/// A dedicated chain rather than rules in OUTPUT directly: a node with a
/// hundred cold sandboxes would otherwise put a hundred rules at the top
/// of OUTPUT, in front of everything else the host does.
const COLD_CHAIN: &str = "SANDSTORM-COLD";

/// Refuse connections to a sandbox that is not running.
///
/// A frozen sandbox keeps its namespace, veth up and nothing answering, so
/// a SYN is dropped rather than rejected and the caller waits out its own
/// timeout. (A snapshotted one no longer has a namespace at all -- see
/// `release` -- and the unreachable floor in `install_rules` refuses for
/// it instead, so this rule is taken off again once it gets there.) Measured before this existed:
/// `curl -m 5` against a snapshotted sandbox took 5011 ms, and `-m 1`
/// took 1019 ms. The duration was purely the caller's deadline.
///
/// `tcp-reset` rather than the default ICMP: a RST is what a client reads
/// as "connection refused" immediately, which is the answer a caller
/// wants.
pub fn reject(ip: Ipv4Addr) -> Result<()> {
    let d = ip.to_string();
    let rule = cold_rule(&d);
    let mut check = vec!["iptables", "-C", COLD_CHAIN];
    check.extend_from_slice(&rule);
    if ok(&check) {
        return Ok(());
    }
    let mut add = vec!["iptables", "-A", COLD_CHAIN];
    add.extend_from_slice(&rule);
    run("rejecting a cold sandbox", &add)?;
    tracing::debug!(%ip, "connections refused while not hot");
    Ok(())
}

/// Let connections through again, once the sandbox is running.
///
/// Idempotent and safe to call for a sandbox that was never rejected,
/// which is what makes it correct to call on every path into Hot rather
/// than only the ones that came from a cold tier.
pub fn allow(ip: Ipv4Addr) -> Result<()> {
    let d = ip.to_string();
    let rule = cold_rule(&d);
    // Delete until it is gone: a repeated add would otherwise leave
    // duplicates that each need removing.
    loop {
        let mut check = vec!["iptables", "-C", COLD_CHAIN];
        check.extend_from_slice(&rule);
        if !ok(&check) {
            break;
        }
        let mut del = vec!["iptables", "-D", COLD_CHAIN];
        del.extend_from_slice(&rule);
        run("allowing a sandbox again", &del)?;
    }
    Ok(())
}

fn cold_rule(dest: &str) -> Vec<&str> {
    vec![
        "-d",
        dest,
        "-p",
        "tcp",
        "-j",
        "REJECT",
        "--reject-with",
        "tcp-reset",
    ]
}

/// Jumped to from FORWARD for everything a sandbox sends: its exceptions, one
/// sandbox's rules after another. What falls off the end is dropped.
const EGRESS_CHAIN: &str = "SANDSTORM-EGRESS";

/// Accepts what is bound for the public internet. A chain of its own so a
/// sandbox with `internet` is one rule in `EGRESS_CHAIN`, not eleven.
const PUBLIC_CHAIN: &str = "SANDSTORM-PUBLIC";

/// What `internet` does not open. The private ranges are the other
/// sandboxes, the host's neighbours and the provider's network; 169.254/16
/// is the cloud metadata service, which holds the instance's credentials.
const NOT_PUBLIC: &[&str] = &[
    "0.0.0.0/8",
    "10.0.0.0/8",
    "100.64.0.0/10",
    "127.0.0.0/8",
    "169.254.0.0/16",
    "172.16.0.0/12",
    "192.0.0.0/24",
    "192.168.0.0/16",
    "198.18.0.0/15",
    "224.0.0.0/4",
    "240.0.0.0/4",
];

/// One sandbox's rules, in the order they are walked: `allow` wins over
/// `deny`, which wins over `internet`. The same arguments serve to add and
/// to delete them.
fn egress_rules(ip: Ipv4Addr, e: &Egress) -> Vec<String> {
    let mut rules = Vec::new();
    for cidr in &e.allow {
        rules.push(format!("-s {ip}/32 -d {cidr} -j ACCEPT"));
    }
    // Pointless with nothing after it to overrule: closed is what the end of
    // the chain already means.
    if e.internet {
        for cidr in &e.deny {
            rules.push(format!("-s {ip}/32 -d {cidr} -j DROP"));
        }
        rules.push(format!("-s {ip}/32 -j {PUBLIC_CHAIN}"));
    }
    rules
}

/// The `iptables-restore` input taking a sandbox from `old` to `new`: one
/// transaction, so it is never left between the two, and a failure changes
/// nothing.
fn egress_script(old: Option<(Ipv4Addr, &Egress)>, ip: Ipv4Addr, new: &Egress) -> String {
    let mut script = String::from("*filter\n");
    if let Some((was, e)) = old {
        for rule in egress_rules(was, e) {
            script.push_str(&format!("-D {EGRESS_CHAIN} {rule}\n"));
        }
    }
    for rule in egress_rules(ip, new) {
        script.push_str(&format!("-A {EGRESS_CHAIN} {rule}\n"));
    }
    script.push_str("COMMIT\n");
    script
}

fn restore(script: &str) -> Result<()> {
    let mut child = Exec::new("iptables-restore")
        .args(["-w", "5", "--noflush"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running iptables-restore")?;
    child
        .stdin
        .take()
        .context("iptables-restore's stdin")?
        .write_all(script.as_bytes())
        .context("writing to iptables-restore")?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!(
            "installing egress rules: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Routing between interfaces, which nothing here turns on until a sandbox
/// needs it: the setting belongs to the host, and a node nobody opens stays
/// as it found it. `install_rules` drops what is not an exception either way.
fn enable_forwarding() -> Result<()> {
    const PATH: &str = "/proc/sys/net/ipv4/ip_forward";
    if std::fs::read_to_string(PATH).is_ok_and(|v| v.trim() == "1") {
        return Ok(());
    }
    std::fs::write(PATH, "1").context("turning on ip_forward")
}

/// The egress rules this node has installed, by sandbox.
///
/// Kept in memory on purpose. `install_rules` starts a node from none, so
/// after a restart this says none as well, the node reports that, and the
/// master sends each sandbox's rules again. What is reported is always what
/// is in the kernel.
#[derive(Default)]
pub struct EgressRules {
    installed: Mutex<HashMap<String, (Ipv4Addr, Egress)>>,
}

impl EgressRules {
    pub fn of(&self, id: &str) -> Egress {
        self.installed
            .lock()
            .get(id)
            .map(|(_, e)| e.clone())
            .unwrap_or_default()
    }

    /// Make `id`'s rules `want`. Runs nothing when the rules would not
    /// change, which is every create that does not ask for egress.
    pub fn set(&self, id: &str, ip: Ipv4Addr, want: &Egress) -> Result<()> {
        let mut installed = self.installed.lock();
        let old = installed.get(id);
        let same = old
            .map(|(was, e)| egress_rules(*was, e))
            .unwrap_or_default()
            == egress_rules(ip, want);
        if !same {
            if !want.is_closed() {
                enable_forwarding()?;
            }
            restore(&egress_script(old.map(|(i, e)| (*i, e)), ip, want))?;
        }
        if *want == Egress::default() {
            installed.remove(id);
        } else {
            installed.insert(id.to_string(), (ip, want.clone()));
        }
        Ok(())
    }

    /// Before its address goes to anyone else: the next holder of the slot
    /// must not inherit what this one was allowed.
    pub fn clear(&self, id: &str) -> Result<()> {
        let mut installed = self.installed.lock();
        if let Some((ip, e)) = installed.get(id) {
            restore(&egress_script(Some((*ip, e)), *ip, &Egress::default()))?;
            installed.remove(id);
        }
        Ok(())
    }
}

/// The rules that make the namespace worth having. Called once at node start.
///
/// `INPUT` accepts established traffic from the range so the node's own
/// connections get their replies, and drops everything else from it, which
/// is what shuts out the host's own services -- sshd listens on 0.0.0.0, so
/// it answers on the veth too. `FORWARD` refuses what a sandbox sends and
/// drops what is sent to one, which covers siblings and the internet without
/// depending on `ip_forward`, except for replies to what an egress exception
/// let out. Those are not checked again: a connection cut by a change of
/// rules dies on the way out.
pub fn install_rules(cfg: &NetConfig) -> Result<()> {
    let subnet = cfg.subnet();
    let rules: &[&[&str]] = &[
        &[
            "-s",
            &subnet,
            "-m",
            "conntrack",
            "--ctstate",
            "ESTABLISHED,RELATED",
            "-j",
            "ACCEPT",
        ],
        &["-s", &subnet, "-j", "DROP"],
    ];
    // Inserted at the top: this host already has a firewall chain on INPUT,
    // and an ACCEPT there would otherwise win.
    for (pos, rule) in rules.iter().enumerate() {
        let mut check = vec!["iptables", "-C", "INPUT"];
        check.extend_from_slice(rule);
        if ok(&check) {
            continue;
        }
        let at = (pos + 1).to_string();
        let mut ins = vec!["iptables", "-I", "INPUT", &at];
        ins.extend_from_slice(rule);
        run("installing an INPUT rule", &ins)?;
    }
    // The chains before the jump to them, and empty: a restarted node has
    // forgotten what was in them, and is told again; see `EgressRules`.
    for chain in [EGRESS_CHAIN, PUBLIC_CHAIN] {
        if !ok(&["iptables", "-n", "-L", chain]) {
            run("creating an egress chain", &["iptables", "-N", chain])?;
        }
        run("clearing an egress chain", &["iptables", "-F", chain])?;
    }
    for cidr in NOT_PUBLIC {
        run(
            "keeping private addresses out of the internet",
            &[
                "iptables",
                "-w",
                "-A",
                PUBLIC_CHAIN,
                "-d",
                cidr,
                "-j",
                "RETURN",
            ],
        )?;
    }
    run(
        "accepting the internet",
        &["iptables", "-A", PUBLIC_CHAIN, "-j", "ACCEPT"],
    )?;
    for (pos, rule) in [
        vec![
            "-d",
            &subnet,
            "-m",
            "conntrack",
            "--ctstate",
            "ESTABLISHED,RELATED",
            "-j",
            "ACCEPT",
        ],
        vec!["-s", &subnet, "-j", EGRESS_CHAIN],
        // Refused rather than dropped: a closed sandbox that looks a name up
        // is told at once, where a drop has it wait out the resolver's
        // timeout, possibly before it serves its first request.
        vec!["-s", &subnet, "-j", "REJECT"],
        vec!["-s", &subnet, "-j", "DROP"],
        vec!["-d", &subnet, "-j", "DROP"],
    ]
    .iter()
    .enumerate()
    {
        let mut check = vec!["iptables", "-C", "FORWARD"];
        check.extend_from_slice(rule);
        if ok(&check) {
            continue;
        }
        let at = (pos + 1).to_string();
        let mut ins = vec!["iptables", "-I", "FORWARD", &at];
        ins.extend_from_slice(rule);
        run("installing a FORWARD rule", &ins)?;
    }
    // What leaves must leave as the node. Harmless while FORWARD lets
    // nothing through.
    let masquerade = [
        "POSTROUTING",
        "-s",
        &subnet,
        "!",
        "-d",
        &subnet,
        "-j",
        "MASQUERADE",
    ];
    let mut check = vec!["iptables", "-t", "nat", "-C"];
    check.extend_from_slice(&masquerade);
    if !ok(&check) {
        let mut add = vec!["iptables", "-t", "nat", "-A"];
        add.extend_from_slice(&masquerade);
        run("installing the masquerade", &add)?;
    }
    // The chain that holds one REJECT per cold sandbox, jumped to from
    // OUTPUT so a connect to a sandbox that is not running is refused
    // rather than left to time out.
    if !ok(&["iptables", "-n", "-L", COLD_CHAIN]) {
        run("creating the cold chain", &["iptables", "-N", COLD_CHAIN])?;
    }
    if !ok(&["iptables", "-C", "OUTPUT", "-j", COLD_CHAIN]) {
        run(
            "jumping to the cold chain",
            &["iptables", "-I", "OUTPUT", "1", "-j", COLD_CHAIN],
        )?;
    }
    // A node that restarted may have left rules for sandboxes it has
    // since forgotten; the tier each one is in is re-derived on the first
    // poll, so start from none rather than from a stale set.
    let _ = run("clearing the cold chain", &["iptables", "-F", COLD_CHAIN]);

    // A floor under the whole range. A sandbox with its namespace up has a
    // /30 route through its veth, which is more specific and wins. One in
    // Snapshot has had its namespace taken down (`release`), and without
    // this its address would follow the default route off the machine --
    // UDP and ICMP and all, possibly to a real host that has that address
    // in the VPC. With it, anything sent there fails at once with "no
    // route to host", which the gateway reads as cold and wakes on, and
    // no per-sandbox rule is needed for the whole Snapshot tier.
    run(
        "installing the unreachable floor",
        &["ip", "route", "replace", "unreachable", &subnet],
    )?;

    tracing::info!(subnet = %subnet, "sandbox network rules in place");
    Ok(())
}

#[cfg(test)]
mod tests {
    /// A repeat of a destroy that has finished. The veth under the sandbox's
    /// name is someone else's by now: here, a dummy of that name. Needs root
    /// to make one.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_destroy_repeated_after_the_namespace_went_leaves_the_veth_alone() {
        use super::*;
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        let cfg = NetConfig::default();
        let id = "repeated-destroy-test";
        let s = cfg.slot(id, 16000).unwrap();
        let link = || {
            Exec::new("ip")
                .args(["-o", "link", "show", &s.host_if])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default()
        };
        run(
            "adding",
            &["ip", "link", "add", &s.host_if, "type", "dummy"],
        )
        .unwrap();
        run("up", &["ip", "link", "set", &s.host_if, "up"]).unwrap();

        quiesce(&cfg, id, 16000).unwrap();
        let up = link().contains(",UP");
        teardown(&cfg, id, 16000, None).unwrap();
        let there = !link().is_empty();
        let _ = run("removing", &["ip", "link", "del", &s.host_if]);

        assert!(up, "a repeat brought it down");
        assert!(there, "a repeat deleted it");
    }

    use super::*;

    #[test]
    fn a_closed_sandbox_has_no_rules() {
        let ip = Ipv4Addr::new(10, 86, 0, 2);
        assert!(egress_rules(ip, &Egress::default()).is_empty());
        // A deny with nothing to overrule would only slow the chain.
        let e = Egress {
            deny: vec!["1.1.1.1/32".into()],
            ..Default::default()
        };
        assert!(egress_rules(ip, &e).is_empty());
        assert!(e.is_closed());
    }

    #[test]
    fn allow_wins_over_deny_which_wins_over_internet() {
        let e = Egress {
            internet: true,
            allow: vec!["10.1.0.0/16".into()],
            deny: vec!["1.1.1.1/32".into()],
        };
        assert_eq!(
            egress_rules(Ipv4Addr::new(10, 86, 0, 2), &e),
            [
                "-s 10.86.0.2/32 -d 10.1.0.0/16 -j ACCEPT",
                "-s 10.86.0.2/32 -d 1.1.1.1/32 -j DROP",
                "-s 10.86.0.2/32 -j SANDSTORM-PUBLIC",
            ]
        );
    }

    /// The internet must never mean the metadata service or a neighbour.
    #[test]
    fn the_internet_is_not_the_private_ranges() {
        for needed in [
            "10.0.0.0/8",
            "169.254.0.0/16",
            "172.16.0.0/12",
            "192.168.0.0/16",
        ] {
            assert!(NOT_PUBLIC.contains(&needed), "{needed}");
        }
    }

    #[test]
    fn a_change_of_rules_is_one_transaction_with_the_old_ones_first() {
        let ip = Ipv4Addr::new(10, 86, 0, 2);
        let open = Egress {
            internet: true,
            ..Default::default()
        };
        let list = Egress {
            allow: vec!["8.8.8.8/32".into()],
            ..Default::default()
        };
        assert_eq!(
            egress_script(Some((ip, &open)), ip, &list),
            "*filter\n\
             -D SANDSTORM-EGRESS -s 10.86.0.2/32 -j SANDSTORM-PUBLIC\n\
             -A SANDSTORM-EGRESS -s 10.86.0.2/32 -d 8.8.8.8/32 -j ACCEPT\n\
             COMMIT\n"
        );
    }

    /// A create that asks for nothing must not run a process: this is on
    /// the path every sandbox takes. `iptables-restore` is not on a test
    /// machine's path either, so a call here would fail the test.
    #[test]
    fn a_closed_sandbox_touches_no_firewall() {
        let rules = EgressRules::default();
        let ip = Ipv4Addr::new(10, 86, 0, 2);
        rules.set("sb", ip, &Egress::default()).unwrap();
        rules.clear("sb").unwrap();
        assert_eq!(rules.of("sb"), Egress::default());
    }

    #[test]
    fn veth_names_are_per_sandbox_not_per_index() {
        let cfg = NetConfig {
            base: "10.86.0.0".parse().unwrap(),
            prefix: 16,
        };
        // Two nodes allocate indices independently, so both hand out 0.
        // Before this was keyed on the id, both sandboxes wanted `sst0h`
        // and `sst0s` -- and a veth's two ends are created in the HOST
        // namespace, so the second one failed with `Cannot find device`.
        let a = cfg.slot("sb-aaaa", 0).unwrap();
        let b = cfg.slot("sb-bbbb", 0).unwrap();
        assert_ne!(a.host_if, b.host_if, "same index, different sandbox");
        assert_ne!(a.sandbox_if, b.sandbox_if);
    }

    #[test]
    fn a_stale_veth_holding_the_address_is_found_and_ours_is_not() {
        let out = "61887: ssteb59b97a8ah    inet 10.86.0.253/30 scope global ssteb59b97a8ah\\       valid_lft forever preferred_lft forever\n\
                   62387: sst7e223fa802h    inet 10.86.0.253/30 scope global sst7e223fa802h\\       valid_lft forever preferred_lft forever\n\
                   2: eth0    inet 10.86.0.253/30 scope global eth0\n";
        assert_eq!(stale_holders(out, "sst7e223fa802h"), ["ssteb59b97a8ah"]);
        assert!(stale_holders(out, "ssteb59b97a8ah").contains(&"sst7e223fa802h".to_string()));
        assert!(stale_holders("", "sstx").is_empty());
    }

    #[test]
    fn veth_names_fit_ifnamsiz() {
        let cfg = NetConfig {
            base: "10.86.0.0".parse().unwrap(),
            prefix: 16,
        };
        // IFNAMSIZ is 16 including the NUL, so 15 usable. A name over that
        // is refused by the kernel at creation time.
        for id in ["sb-0", &format!("sb-{}", "f".repeat(64))] {
            let s = cfg.slot(id, 3).unwrap();
            assert!(s.host_if.len() <= 15, "{} is too long", s.host_if);
            assert!(s.sandbox_if.len() <= 15);
        }
    }

    #[test]
    fn the_same_sandbox_always_gets_the_same_names() {
        let cfg = NetConfig {
            base: "10.86.0.0".parse().unwrap(),
            prefix: 16,
        };
        // A restart has to find the interfaces it created last time.
        assert_eq!(
            cfg.slot("sb-stable", 7).unwrap().host_if,
            cfg.slot("sb-stable", 9).unwrap().host_if,
            "the name must not move when the index does"
        );
    }

    #[test]
    fn slots_are_four_apart_and_do_not_overlap() {
        let c = NetConfig::default();
        let a = c.slot("sb-a", 0).unwrap();
        let b = c.slot("sb-b", 1).unwrap();
        assert_eq!(a.host_ip, Ipv4Addr::new(10, 86, 0, 1));
        assert_eq!(a.sandbox_ip, Ipv4Addr::new(10, 86, 0, 2));
        assert_eq!(b.host_ip, Ipv4Addr::new(10, 86, 0, 5));
        assert_eq!(b.sandbox_ip, Ipv4Addr::new(10, 86, 0, 6));
    }

    /// Each sandbox is alone on its /30, so there is no shared segment for
    /// siblings to talk over in the first place.
    #[test]
    fn no_two_slots_share_an_address() {
        let c = NetConfig::default();
        let mut seen = std::collections::HashSet::new();
        for i in 0..2000 {
            let s = c.slot("sb", i).unwrap();
            assert!(seen.insert(s.host_ip), "host ip reused at {i}");
            assert!(seen.insert(s.sandbox_ip), "sandbox ip reused at {i}");
        }
    }

    /// The kernel truncates an interface name over 15 characters, which
    /// would silently collide two sandboxes onto one device.
    #[test]
    fn interface_names_fit_the_kernel_limit() {
        let c = NetConfig::default();
        for i in [0u32, 1, 999, 16383] {
            let s = c.slot("sb-0123456789ab", i).unwrap();
            assert!(s.host_if.len() <= 15, "{} too long", s.host_if);
            assert!(s.sandbox_if.len() <= 15, "{} too long", s.sandbox_if);
            assert_ne!(s.host_if, s.sandbox_if);
        }
    }

    #[test]
    fn running_out_of_addresses_is_an_error_not_a_wrap() {
        let c = NetConfig::default();
        // A /16 holds 16384 /30s, so 16383 is the last one.
        assert!(c.slot("sb", 16383).is_ok());
        assert!(c.slot("sb", 16384).is_err());
        let e = c.slot("sb", 99_999).unwrap_err().to_string();
        assert!(e.contains("out of addresses"), "got {e}");
    }

    /// The namespace is named after the sandbox so a restarted node can
    /// find it from the id alone; the interfaces are named after the slot
    /// so they fit the kernel's 15-character limit.
    /// A RST, not an ICMP unreachable: a client reads a reset as
    /// "connection refused" at once, which is the whole point.
    #[test]
    fn the_cold_rule_resets_rather_than_dropping() {
        let r = cold_rule("10.86.0.2");
        assert!(r.contains(&"REJECT"));
        assert!(
            r.contains(&"tcp-reset"),
            "a drop would make the caller wait"
        );
        assert_eq!(r[0], "-d");
        assert_eq!(r[1], "10.86.0.2");
    }

    #[test]
    fn the_namespace_is_named_for_the_sandbox() {
        let c = NetConfig::default();
        let s = c.slot("sb-deadbeef0123", 42).unwrap();
        assert_eq!(s.netns, "sandstorm-sb-deadbeef0123");
        // The interface names used to be `sst42h`/`sst42s`, from the index.
        // They are keyed on the sandbox now -- see `if_tag` -- because the
        // index is per node and two nodes on one machine collided.
        assert!(s.host_if.starts_with("sst") && s.host_if.ends_with('h'));
        assert!(
            !s.host_if.contains("42"),
            "{} still carries the index",
            s.host_if
        );
    }

    fn pool(key: &str) -> Pool {
        Pool {
            target: 0,
            tag: if_tag(key)[..6].to_string(),
            prepare: Box::new(|_| Ok(())),
            state: Default::default(),
            wanted: Default::default(),
        }
    }

    #[test]
    fn pooled_names_fit_ifnamsiz_and_stay_apart() {
        let p = pool("/var/lib/sandstorm");
        for n in [1u64, 0xff_ffff, 1 << 40] {
            let b = p.blank(n);
            assert!(b.host_if.len() <= 15, "{} too long", b.host_if);
            assert!(b.sandbox_if.len() <= 15, "{} too long", b.sandbox_if);
            assert!(b.netns.starts_with(&p.netns_prefix()));
        }
        assert_ne!(p.blank(1), p.blank(2));
        // Two nodes on one machine: neither may take, or clear at start,
        // the other's.
        let q = pool("/var/lib/sandstorm-2");
        assert_ne!(p.netns_prefix(), q.netns_prefix());
        assert_ne!(p.blank(1).host_if, q.blank(1).host_if);
        // Nor collide with a sandbox's own names.
        assert!(!p.blank(1).host_if.starts_with("sst"));
    }

    #[test]
    fn a_stale_route_for_the_slot_is_found_and_ours_is_not() {
        // From bm: two /30s, a default route, and a host end left behind
        // routing 10.86.0.0/30 as well as ours.
        let table =
            "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
eth0\t00000000\t0110150A\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
sst9244ce5dc8h\t0000560A\t00000000\t0001\t0\t0\t0\tFCFFFFFF\t0\t0\t0\n\
ssteb59b97a8ah\t0000560A\t00000000\t0001\t0\t0\t0\tFCFFFFFF\t0\t0\t0\n\
sp09c66b00000ah\t0000560A\t00000000\t0001\t0\t0\t0\tFCFFFFFF\t0\t0\t0\n\
sst0000000000h\t0400560A\t00000000\t0001\t0\t0\t0\tFCFFFFFF\t0\t0\t0\n";
        let ip = Ipv4Addr::new(10, 86, 0, 1);
        // A pooled namespace's host end keeps the pool's name.
        assert_eq!(
            route_holders(table, ip, "sst9244ce5dc8h"),
            ["ssteb59b97a8ah", "sp09c66b00000ah"]
        );
        assert!(route_holders(table, Ipv4Addr::new(10, 86, 0, 9), "x").is_empty());
        assert!(route_holders("", ip, "x").is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ioctl_requests_carry_the_name_and_address_as_the_kernel_reads_them() {
        let r = ifconf::ifreq("sst0123456789h").unwrap();
        let name: Vec<u8> = r
            .ifr_name
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        assert_eq!(name, b"sst0123456789h");
        assert!(
            ifconf::ifreq("sst0123456789abc").is_err(),
            "15 characters is the limit"
        );
        let a = ifconf::sockaddr(Ipv4Addr::new(10, 86, 0, 21));
        assert_eq!(a.sa_family as i32, libc::AF_INET);
        // Port (2 bytes) then the address in network order.
        assert_eq!(
            &a.sa_data[2..6].iter().map(|&b| b as u8).collect::<Vec<_>>(),
            &[10, 86, 0, 21]
        );
    }

    #[test]
    fn the_pool_waits_out_a_claim_before_refilling() {
        let p = pool("/w");
        assert_eq!(p.quiet_left(), None, "nothing taken, nothing to wait for");
        p.take();
        let left = p.quiet_left().expect("just taken");
        assert!(left <= REFILL_QUIET && left > REFILL_QUIET / 2);
        p.state.lock().unwrap().last_take =
            Some(std::time::Instant::now() - REFILL_QUIET - std::time::Duration::from_millis(1));
        assert_eq!(p.quiet_left(), None, "quiet long enough");
    }

    #[test]
    fn a_pool_running_low_stops_waiting_for_quiet() {
        let mut p = pool("/w");
        p.target = 32;
        let fill =
            |p: &Pool, n: u64| p.state.lock().unwrap().ready = (0..n).map(|i| p.blank(i)).collect();
        fill(&p, 32);
        assert_eq!(p.pressed(), (false, false));
        fill(&p, 8);
        assert_eq!(
            p.pressed(),
            (false, false),
            "a quarter left is still enough"
        );
        fill(&p, 7);
        assert_eq!(p.pressed(), (true, true));
        fill(&p, 32);
        p.state.lock().unwrap().retired = (0..=MAX_RETIRED as u64).map(|i| p.blank(i)).collect();
        assert_eq!(p.pressed(), (false, true), "destroys outran it");
        fill(&p, 31);
        p.state.lock().unwrap().retired = vec![p.blank(99)];
        assert_eq!(p.pressed(), (false, true), "one to reuse");
        assert!(p.reusable());
    }

    #[test]
    fn an_empty_pool_hands_out_nothing() {
        let p = pool("/w");
        assert!(p.take().is_none());
        p.state.lock().unwrap().ready.push(p.blank(1));
        assert_eq!(p.take(), Some(p.blank(1)));
        assert!(p.take().is_none());
    }

    #[test]
    fn a_smaller_range_holds_fewer_sandboxes() {
        let c = NetConfig {
            base: Ipv4Addr::new(10, 200, 0, 0),
            prefix: 24,
        };
        assert!(c.slot("sb", 63).is_ok(), "a /24 holds 64 slots");
        assert!(c.slot("sb", 64).is_err());
        assert_eq!(c.subnet(), "10.200.0.0/24");
    }
}
