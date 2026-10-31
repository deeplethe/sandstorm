//! What the master and a node say to each other.
//!
//! The node dials out and asks for work; the master answers on that
//! connection. There is no master -> node connect anywhere in this protocol,
//! which is why a node needs no inbound port.

use serde::{Deserialize, Serialize};

use crate::tier::Tier;

/// What a node's fingerprint is made of, so an operator can see why two
/// nodes landed in different pools instead of comparing two hashes. Read in
/// the same pass as the fingerprint (`node::runtime::host`), which hashes the
/// engine, kernel, vendor and flags; `cpu_model` is shown but not hashed,
/// because the same feature set under another model name restores fine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInfo {
    /// `Backend::version`, e.g. `runsc version release-20261019.0`.
    pub engine: String,
    /// `/proc/sys/kernel/osrelease`.
    pub kernel: String,
    pub cpu_vendor: String,
    pub cpu_model: String,
    /// The `flags` line of `/proc/cpuinfo`, space separated.
    pub cpu_flags: String,
}

/// Node -> master, once, to trade a registration token for a certificate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub token: String,
    pub node_id: String,
    /// What this node is compatible with. Snapshots are only ever restored
    /// on a node with the same value; measured on bj/hk, a mismatch fails
    /// with `incompatible FeatureSet` and a match is still not a guarantee.
    pub fingerprint: String,
    pub max_sandboxes: u32,
    /// What `fingerprint` was computed from. Absent from older nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterResponse {
    pub node_id: String,
    pub pool: String,
    /// PEM. The node writes these and uses them for every later call.
    pub client_cert: String,
    pub client_key: String,
    pub ca_cert: String,
    /// Bearer token for the node's later calls. Empty when the master is
    /// running unauthenticated.
    #[serde(default)]
    pub api_token: String,
}

// ---- listen mode: the master connects to the node ----------------------
//
// The same messages as the poll, carried the other way. A node that listens
// reports a `PollRequest` as its answer and is handed commands in the
// request, so the master's reconciliation runs unchanged whichever side
// opened the connection.

/// What a listening node says about itself when a master adds it. The same
/// facts registration carries in pull mode, minus the token: the pairing
/// code has already proved who is asking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentHello {
    pub node_id: String,
    /// Host fingerprint, which decides the pool exactly as it does when a
    /// node registers.
    pub fingerprint: String,
    pub max_sandboxes: u32,
    /// What `fingerprint` was computed from. Absent from older nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostInfo>,
}

/// What a master sends to a node it drives.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncRequest {
    /// `None` asks for a report and changes nothing. `Some` is a full batch:
    /// the node applies it and then forgets failures for any sandbox the
    /// batch did not mention, because the master has stopped asking about
    /// those. The distinction matters -- an empty list would wipe the
    /// failure book on every report-only refresh, and no sandbox would ever
    /// reach quarantine.
    #[serde(default)]
    pub commands: Option<Vec<Command>>,
}

/// What a listening node answers with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResponse {
    /// State AFTER applying the batch, so the master's next diff is computed
    /// against what is actually there now.
    pub report: PollRequest,
    /// Answers to exec and branch commands in the batch.
    #[serde(default)]
    pub results: Vec<CommandResult>,
    /// When the node next wants to retry something that is backing off, so
    /// the master's driver can come back then rather than on its refresh tick.
    #[serde(default)]
    pub retry_in_ms: Option<u64>,
}

/// Node -> master: "here is what I actually hold, give me work."
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollRequest {
    pub node_id: String,
    /// Observed, not remembered. The master compares this against desired
    /// state rather than trusting its own cache.
    pub observed: Vec<ObservedSandbox>,
    pub free_memory_mib: u64,
    pub pressure_some_avg10: f64,
    /// How long the master may park this request when there is no work.
    /// The node sets it, shortening it when it has a retry due, so it can
    /// stay parked rather than sleeping. 0 means answer at once, which is
    /// what tests and one-shot probes want.
    #[serde(default)]
    pub hold_ms: Option<u64>,
    /// Commands that failed since the last call. Reported so the reason a
    /// sandbox is not coming up is visible in the API rather than only in
    /// one node's log.
    #[serde(default)]
    pub failures: Vec<CommandFailure>,
    /// What the node is putting off, and for how long: a retry that is not
    /// due, a demotion inside its grace, a fork waiting for its image. The
    /// master would only hand these straight back. On bare metal one failing
    /// sandbox had a node polling about 350 times a second, 40% of a core,
    /// until its retry came up.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deferred: Vec<Deferred>,
    /// Sandboxes the gateway forwarded a request to since the last report.
    ///
    /// The gateway wakes a cold sandbox itself when a request arrives for
    /// it (RFC §15.5: a wake never goes through the master), so this is how
    /// the master learns the sandbox is in use: it moves the idle clock and,
    /// for one that was woken, makes Hot the desired tier so the next diff
    /// does not put it straight back to sleep.
    #[serde(default)]
    pub traffic: Vec<String>,
    /// Sandboxes this node put to disk on its own, to make room for a wake.
    ///
    /// The master takes the node's word for these and moves desired state
    /// to the tier observed. If it asked for them back instead, the node
    /// would make room for that by putting away something else, and the
    /// two would take turns for ever.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evicted: Vec<String>,
    /// The density levers this node has on (`snapshot-compression`,
    /// `reclaim-hot`, `swap:zswap`, ...). Each node decides for itself from
    /// what its machine can do, so nodes in one pool can differ, and this is
    /// where that shows: `sandstorm_node_feature` on the master's /metrics.
    #[serde(default)]
    pub features: Vec<String>,
    /// Snapshot tags whose images are on this node's disk, baked here or
    /// fetched from another node. The master drops the ones it no longer
    /// has a record of, which is the only way an image ever leaves a node.
    /// A tag only appears here once its image is some minutes old, so one
    /// that a branch has just written -- before the master has recorded it
    /// -- is never mistaken for one that was deleted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// What this node's fingerprint is made of. Sent with every full report
    /// rather than only at registration, so nodes registered before it
    /// existed fill it in too; the master writes it only when it changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostInfo>,
    /// This node's fingerprint now. Registration records it once, but an
    /// upgrade of runsc or the kernel changes it under a node that is already
    /// registered, and the master has to move that node out of a pool whose
    /// snapshots it can no longer restore (ADR 6). Absent from older nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// Templates this node can start on its own backend: those with a rootfs
    /// in the form that backend reads (`templates/<name>/rootfs` for runsc,
    /// `templates/<name>/rootfs.ext4` for Firecracker). The master places a
    /// template's sandboxes only on nodes that list it, so a create a group
    /// cannot run is refused rather than failed on a node. Absent from older
    /// nodes, which are placed on as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub templates: Option<Vec<String>>,
    /// Where each image pull on this node stands.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pulls: Vec<TemplatePull>,
}

/// How an image says to run it, read from its config.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageConfig {
    /// `Entrypoint` then `Cmd`.
    pub args: Vec<String>,
    pub env: Vec<String>,
    pub cwd: Option<String>,
    /// The lowest port it exposes.
    pub port: Option<u16>,
}

/// One template being pulled from an image, or the last word on one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplatePull {
    pub name: String,
    pub image: String,
    /// Which attempt this is. A retry is a new number, so the outcome of an
    /// old attempt is not taken for the new one's.
    #[serde(default)]
    pub attempt: u32,
    pub done: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<ImageConfig>,
}

impl TemplatePull {
    pub fn running(name: &str, image: &str, attempt: u32) -> Self {
        Self {
            name: name.into(),
            image: image.into(),
            attempt,
            done: false,
            error: None,
            config: None,
        }
    }

    pub fn done(name: &str, image: &str, attempt: u32, config: ImageConfig) -> Self {
        Self {
            done: true,
            config: Some(config),
            ..Self::running(name, image, attempt)
        }
    }

    pub fn failed(name: &str, image: &str, attempt: u32, error: String) -> Self {
        Self {
            done: true,
            error: Some(error),
            ..Self::running(name, image, attempt)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Deferred {
    pub id: String,
    pub retry_in_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandFailure {
    pub id: String,
    pub error: String,
    /// How many times in a row this sandbox has failed. The node backs off
    /// on its own; this tells the master how bad it is.
    pub attempts: u32,
    /// Not a failure: the node held the command back for want of memory,
    /// and will start it once there is room. The master shows the reason
    /// but does not count it towards quarantine -- on 2026-10-27, 1,246 of
    /// 20,000 creates at 20/s were quarantined this way and never started,
    /// on nodes that had room for them minutes later.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub held_back: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservedSandbox {
    pub id: String,
    pub tier: Tier,
    pub serving: bool,
    /// The address the node gave it. The node owns addressing, so this is
    /// what the master records and serves.
    #[serde(default)]
    pub ip: Option<String>,
    /// Which slot it holds, read back from the node's own records so an
    /// orphan can have its address reclaimed.
    #[serde(default)]
    pub net_index: Option<u32>,
    /// The egress rules in force for it, read from what the node installed.
    /// A restarted node has none, which is how the master knows to send
    /// them again. Absent from a node too old to have any.
    #[serde(default)]
    pub egress: Option<Egress>,
}

/// What a sandbox may reach beyond its own node. The default, nothing, is
/// what every sandbox had before this existed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Egress {
    /// The public internet: everything but private, link-local and
    /// otherwise unroutable addresses (`NOT_PUBLIC` in `node/net.rs`).
    #[serde(default)]
    pub internet: bool,
    /// IPv4 addresses and CIDRs always reachable, private ones included.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    /// IPv4 addresses and CIDRs never reachable, unless `allow` names them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
}

/// More than this is a mistake or a policy that belongs in a proxy: each
/// entry is one firewall rule every new connection from the sandbox walks.
const MAX_EGRESS_ENTRIES: usize = 64;

impl Egress {
    /// Whether it opens nothing, and so needs no rule on the node.
    pub fn is_closed(&self) -> bool {
        !self.internet && self.allow.is_empty()
    }

    /// Every entry as a canonical CIDR, so what the master stores compares
    /// equal to what a node reports, or why it cannot be.
    pub fn canonical(self) -> Result<Self, String> {
        if self.allow.len() + self.deny.len() > MAX_EGRESS_ENTRIES {
            return Err(format!(
                "at most {MAX_EGRESS_ENTRIES} allow and deny entries, not {}",
                self.allow.len() + self.deny.len()
            ));
        }
        let list = |l: Vec<String>| {
            let mut out: Vec<String> = Vec::with_capacity(l.len());
            for entry in l {
                let c = canonical_cidr(&entry)?;
                if !out.contains(&c) {
                    out.push(c);
                }
            }
            Ok::<_, String>(out)
        };
        Ok(Self {
            internet: self.internet,
            allow: list(self.allow)?,
            deny: list(self.deny)?,
        })
    }
}

fn canonical_cidr(entry: &str) -> Result<String, String> {
    let (addr, prefix) = entry.trim().split_once('/').unwrap_or((entry.trim(), "32"));
    let why = || format!("{entry:?} is not an IPv4 address or CIDR; domains are not supported");
    let addr: std::net::Ipv4Addr = addr.parse().map_err(|_| why())?;
    let prefix: u32 = prefix.parse().ok().filter(|p| *p <= 32).ok_or_else(why)?;
    let mask = u32::MAX.checked_shl(32 - prefix).unwrap_or(0);
    Ok(format!(
        "{}/{prefix}",
        std::net::Ipv4Addr::from(u32::from(addr) & mask)
    ))
}

/// Master -> node, in reply to a poll. Never a push.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollResponse {
    pub commands: Vec<Command>,
}

/// A command the node should run once, as opposed to a tier the master
/// wants it to reach.
///
/// Tier changes are declarative: the master says where a sandbox should be
/// and the node converges on it, so a lost command is simply re-derived on
/// the next poll. `Exec` is not like that -- it runs once and a caller is
/// holding an HTTP request open waiting for its output. So it is queued and
/// delivered, not computed, and if it is lost the caller sees a timeout
/// rather than the command running twice.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Command {
    /// Bring a sandbox up from nothing.
    ///
    /// The spec travels with the command so a template is defined once, on
    /// the master. The rootfs does not: `template` only names a directory
    /// the node already has, so a master cannot point a node at an
    /// arbitrary path on its own disk.
    Create {
        id: String,
        template: String,
        spec: serde_json::Value,
        /// Port inside the sandbox. Not a host port any more: each sandbox
        /// has its own network namespace, so two may both use 8080.
        port: u16,
        volume: Option<String>,
        /// Slot in the node's address plan, which fixes the sandbox's IP.
        net_index: Option<u32>,
        /// Fork from this tag's checkpoint instead of starting cold. One
        /// image serves any number of children, so this is a restore, not
        /// a copy: measured at 45 ms each, five at once from one 14 MiB
        /// image.
        from_tag: Option<String>,
        /// Host port the node's gateway listens on for this sandbox.
        ///
        /// Unlike `port` this one IS a host port, so it is allocated per
        /// node and unique there. `None` leaves the sandbox reachable only
        /// from its own node, which is what every sandbox was before the
        /// gateway existed.
        #[serde(default)]
        gateway_port: Option<u16>,
        /// The sandbox's own access token, so the node's gateway can check
        /// it without asking the master on the data path -- RFC 15.5 is
        /// explicit that a request must not travel through the master.
        /// Carried over the mTLS control channel and stored at 0600.
        #[serde(default)]
        token: String,
        /// Where to fetch `from_tag`'s image if this node does not have it.
        /// Absent when the image is here, or for a cold start.
        /// Boxed: rarely present, and it would otherwise make every command
        /// as large as the largest.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fetch: Option<Box<FetchFrom>>,
        /// What it may reach. Here as well as in `SetEgress` so a sandbox
        /// that fetches something as it starts has the network it needs.
        #[serde(default)]
        egress: Egress,
    },
    /// Have this template, pulled from this image if it is not here. Sent
    /// while the node reports no pull of this attempt, so it is declarative.
    PullTemplate {
        name: String,
        image: String,
        attempt: u32,
    },
    /// Replace what a sandbox may reach. Declarative, like `SetTier`: the
    /// master sends it while a node reports anything else.
    SetEgress { id: String, egress: Egress },
    /// Move an existing sandbox to a tier. One step at a time when demoting;
    /// promotion goes straight to hot.
    SetTier { id: String, tier: Tier },
    /// Remove it and everything it owns on this node, its network included.
    Destroy { id: String, net_index: Option<u32> },
    /// Checkpoint a running sandbox into a named tag, then put it back.
    ///
    /// runsc's checkpoint stops the container, so the parent is restored
    /// from the image it just wrote. That is a pause, not a live fork --
    /// the RFC lists a live branch's pause window as a non-goal, and this
    /// is the honest version of that trade.
    Branch {
        id: String,
        tag: String,
        /// Correlates the result with the caller waiting on it, exactly as
        /// for Exec.
        result_id: String,
    },
    /// Remove a tag's image from this node. Issued for a tag the node
    /// reports holding that the master no longer has.
    DropTag { tag: String },
    /// Run a command inside a sandbox and report back. `exec_id` is how the
    /// master matches the result to the caller still waiting on it.
    Exec {
        exec_id: String,
        id: String,
        args: Vec<String>,
        timeout_secs: u64,
    },
    /// Restart this node on another backend (`runsc` or `firecracker`).
    ///
    /// Only sent to a node that says it can (`backend-switch` in its
    /// features: it was installed with `--backend-file`), and only while the
    /// master holds no sandbox or snapshot on it -- every one of those is in
    /// the old backend's format and could never be restored. The node checks
    /// again for itself, runs the new backend's preflight, rewrites the file
    /// and exits for systemd to start it again; its fingerprint then names
    /// the new engine and it moves to that engine's pool. Answered like
    /// Branch, before the restart.
    SetBackend { backend: String, result_id: String },
}

/// Where a node fetches a tag's image from: the node that baked it.
///
/// That node's agent port, pinned to its certificate the same way the
/// master pins it, and a token good for this tag, this fetching node and
/// until `expires` (see `tag::fetch_token`). Not the pairing code, which
/// would let one node drive another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchFrom {
    pub node_id: String,
    pub address: String,
    pub cert_sha256: String,
    /// The fetching node, which the token is bound to.
    pub for_node: String,
    pub expires: i64,
    pub token: String,
}

/// Node -> master, out of band: the outcome of one command a caller is
/// waiting on. Both `Exec` and `Branch` come back this way.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandResult {
    /// The `exec_id` or `result_id` from the command.
    #[serde(alias = "exec_id")]
    pub result_id: String,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    pub exit_code: i64,
    /// Bytes the branch's image occupies, for a Branch.
    #[serde(default)]
    pub size_bytes: u64,
}

#[cfg(test)]
mod egress_tests {
    use super::*;

    fn with(allow: &[&str], deny: &[&str]) -> Egress {
        Egress {
            internet: false,
            allow: allow.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Stored and reported forms have to compare equal, or the master
    /// sends the same rules for ever.
    #[test]
    fn entries_become_canonical_cidrs() {
        let e = with(&["8.8.8.8", " 10.1.2.3/8 ", "10.0.0.0/8"], &["0.0.0.0/0"])
            .canonical()
            .unwrap();
        assert_eq!(e.allow, ["8.8.8.8/32", "10.0.0.0/8"]);
        assert_eq!(e.deny, ["0.0.0.0/0"]);
    }

    #[test]
    fn what_is_not_ipv4_is_refused_by_name() {
        for bad in [
            "example.com",
            "::1",
            "1.2.3.4/33",
            "1.2.3",
            "1.2.3.4/x",
            "-j ACCEPT",
        ] {
            let why = with(&[bad], &[]).canonical().unwrap_err();
            assert!(why.contains("not an IPv4"), "{bad}: {why}");
        }
    }

    #[test]
    fn a_policy_too_long_to_walk_is_refused() {
        let many: Vec<String> = (0..65).map(|i| format!("10.0.0.{i}")).collect();
        let e = Egress {
            allow: many,
            ..Default::default()
        };
        assert!(e.canonical().unwrap_err().contains("at most 64"));
    }

    /// A node from before egress reports none; old rows and commands have
    /// none. Both must read as closed, not fail to parse.
    #[test]
    fn absent_means_closed() {
        let o: ObservedSandbox =
            serde_json::from_str(r#"{"id":"a","tier":"hot","serving":true}"#).unwrap();
        assert_eq!(o.egress, None);
        assert!(Egress::default().is_closed());
    }
}
