//! Firecracker, the optional backend for hosts with `/dev/kvm`.
//!
//! Upstream Firecracker, no fork. What it keeps from v0.5:
//!
//!   - Forks share one page cache: every child of a tag maps the same
//!     memory file with the File backend, which is `MAP_PRIVATE`.
//!   - Diff snapshots: a sandbox restored from an image is snapshotted next
//!     time as only the pages it dirtied, chained onto that image.
//!   - A private disk per sandbox, reflinked, so no two VMs ever write one
//!     ext4 image.
//!
//! An image is a flat directory -- tags are transferred as flat files:
//!
//! ```text
//!   vmstate          devices and vCPUs
//!   mem.0            full memory
//!   mem.1 .. mem.k   diffs, sparse, each against everything before it
//!   rootfs.ext4      the disk as it was while the VM was paused
//!   .mem.assembled   mem.0 with the diffs applied; a local cache, never sent
//! ```
//!
//! Memory and disk travel together because the guest's page cache refers to
//! disk blocks: memory from one moment over the disk from another corrupts
//! the filesystem without any error at the time.
//!
//! The disk's path in the VM is the relative `rootfs.ext4`, and Firecracker
//! runs with the sandbox's own disk directory as its working directory. So
//! the path recorded in a vmstate resolves, in whichever sandbox loads it,
//! to that sandbox's own copy -- never the parent's, and with no need for
//! the parent's path to exist on this node.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command as Exec, Stdio};
use std::time::{Duration, Instant};

use super::backend::Backend;
use super::bundle::{self, BuildRequest, TemplateSpec};
use super::net;
use super::runtime::RuntimeConfig;

/// Every guest has the same address, behind a tap in its own namespace, so
/// the network a snapshot remembers is right in any namespace it is
/// restored into. The namespace's own address is DNATed to it.
const GUEST_IP: &str = "10.42.0.2";
const TAP_CIDR: &str = "10.42.0.1/30";
const TAP: &str = "tap0";
const GUEST_MAC: &str = "AA:FC:00:00:00:01";
/// The tap's, fixed for the same reason as the guest's: a restored guest
/// still has the tap it was snapshotted beside in its ARP cache, and every
/// sandbox has a tap of its own. With a random MAC per tap, a restored guest
/// sent to the seed's until its entry went stale: warm starts took 43 s on
/// bare metal once the guest's own entry was pinned and nothing asked it.
const TAP_MAC: &str = "AA:FC:00:00:00:02";

/// The guest agent (`rootfs-init/sandstorm-agent.py`). A template cannot
/// use this port.
const AGENT_PORT: u16 = 8888;

/// In a sandbox's bundle when its VM was restored, naming the memory file
/// it maps: its writes are tracked by the kernel's soft-dirty bits, not by
/// KVM. See `track_writes`.
const SOFT_DIRTY: &str = "soft-dirty";

/// Diffs on top of a full image before the next checkpoint starts again
/// from a full one. Each link costs a pass over its data at assembly.
const MAX_CHAIN: usize = 8;

/// Guest memory when the template does not say.
const DEFAULT_MEM_MIB: u64 = 512;
/// The cgroup's ceiling is the guest's memory plus this, for the VMM.
const VMM_OVERHEAD_MIB: u64 = 64;
/// x86's COMMAND_LINE_SIZE.
const CMDLINE_MAX: usize = 2048;

/// Calls that should answer at once.
const API_TIMEOUT: Duration = Duration::from_secs(10);
/// Snapshot create and load do work proportional to memory.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(120);

pub struct Firecracker {
    pub bin: String,
    /// Uncompressed vmlinux.
    pub kernel: PathBuf,
    pub vcpus: u8,
    pub work_dir: PathBuf,
    /// Boot guests with a serial console; see `pvm_host`.
    pub serial_console: bool,
}

/// What `prepare` records in `bundles/<id>/config.json`, so every later
/// call can be made from the id alone after a restart.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct VmConfig {
    /// The network namespace's name, as `ip netns` knows it.
    netns: Option<String>,
    /// The namespace's address, which the guest is reached at.
    sandbox_ip: Option<Ipv4Addr>,
    mem_mib: u64,
    vcpus: u8,
    boot_args: String,
}

impl Firecracker {
    /// Refuse to run where this backend cannot work, rather than fail on
    /// every sandbox. ADR 11: no silent full copies.
    pub fn check_host(&self) -> Result<()> {
        if !Path::new("/dev/kvm").exists() {
            bail!("the firecracker backend needs /dev/kvm, and this host has none");
        }
        if !self.kernel.is_file() {
            bail!("no guest kernel at {}", self.kernel.display());
        }
        let probe = self.work_dir.join("tmp").join("reflink-probe");
        std::fs::create_dir_all(probe.parent().unwrap())?;
        std::fs::write(&probe, b"probe")?;
        let copy = probe.with_extension("copy");
        let r = reflink(&probe, &copy);
        std::fs::remove_file(&probe).ok();
        std::fs::remove_file(&copy).ok();
        r.with_context(|| {
            format!(
                "{} cannot reflink; the firecracker backend needs XFS (reflink=1) \
                 or btrfs, or every sandbox would copy its whole disk",
                self.work_dir.display()
            )
        })
    }

    fn bundle(&self, id: &str) -> PathBuf {
        self.work_dir.join("bundles").join(id)
    }

    fn disk_dir(&self, id: &str) -> PathBuf {
        self.work_dir.join("layers").join(id).join("disk")
    }

    /// The images this VM's memory came from, hard-linked: what the next
    /// checkpoint can be a diff against.
    fn base_dir(&self, id: &str) -> PathBuf {
        self.work_dir.join("layers").join(id).join("base")
    }

    fn sock(&self, id: &str) -> PathBuf {
        self.bundle(id).join("fc.sock")
    }

    fn pid(&self, id: &str) -> Result<u32> {
        let p = self.bundle(id).join("fc.pid");
        std::fs::read_to_string(&p)
            .with_context(|| format!("reading {}", p.display()))?
            .trim()
            .parse()
            .with_context(|| format!("{} is not a pid", p.display()))
    }

    /// The memory file a restored VM maps, if its writes are tracked by
    /// soft-dirty bits.
    fn tracked(&self, id: &str) -> Option<MappedFile> {
        let raw = std::fs::read_to_string(self.bundle(id).join(SOFT_DIRTY)).ok()?;
        serde_json::from_str(&raw).ok()
    }

    fn config(&self, id: &str) -> Result<VmConfig> {
        let p = self.bundle(id).join("config.json");
        let raw = std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
        serde_json::from_slice(&raw).with_context(|| format!("{} is not a VM config", p.display()))
    }

    /// Start Firecracker for this sandbox, detached from this process and
    /// inside the sandbox's cgroup from its first instruction.
    fn spawn(&self, id: &str, cfg: &VmConfig) -> Result<()> {
        let sock = self.sock(id);
        let _ = std::fs::remove_file(&sock);
        let disk = self.disk_dir(id);
        let logs = self.work_dir.join("logs");
        std::fs::create_dir_all(&logs)?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(logs.join(format!("{id}.log")))?;

        let cg = cgroup(id, cfg.mem_mib + VMM_OVERHEAD_MIB)?;
        let procs = std::fs::OpenOptions::new()
            .write(true)
            .open(cg.join("cgroup.procs"))
            .with_context(|| format!("opening {}/cgroup.procs", cg.display()))?;

        // Its namespace is entered in the child before exec, not through
        // `ip netns exec`: that is another process, and it remounts /sys
        // for a program that only needs the namespace's tap. About 10 ms
        // of a warm start on bm went on starting Firecracker.
        let netns = cfg
            .netns
            .as_ref()
            .map(|ns| std::fs::File::open(format!("/var/run/netns/{ns}")))
            .transpose()
            .with_context(|| format!("opening {id}'s network namespace"))?;
        let mut cmd = Exec::new(&self.bin);
        cmd.arg("--api-sock")
            .arg(&sock)
            .current_dir(&disk)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        {
            use std::os::unix::io::AsRawFd;
            use std::os::unix::process::CommandExt;
            let fd = procs.as_raw_fd();
            let ns_fd = netns.as_ref().map(|f| f.as_raw_fd());
            // Its own session, so nothing aimed at this process's group
            // reaches it; into the cgroup before exec, so there is no
            // moment when a freeze or a kill would miss it; and into its
            // network namespace.
            unsafe {
                cmd.pre_exec(move || {
                    if libc::setsid() < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::write(fd, b"0".as_ptr().cast(), 1) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if let Some(ns) = ns_fd {
                        #[cfg(target_os = "linux")]
                        if libc::setns(ns, libc::CLONE_NEWNET) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        #[cfg(not(target_os = "linux"))]
                        {
                            let _ = ns;
                            return Err(std::io::ErrorKind::Unsupported.into());
                        }
                    }
                    Ok(())
                });
            }
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("starting {} for {id}", self.bin))?;
        drop(procs);
        drop(netns);
        let pid = child.id();
        std::fs::write(self.bundle(id).join("fc.pid"), pid.to_string())?;
        // Reaped here while this process lives; after a restart it is
        // init's to reap. Never killed on drop: a node restart must leave
        // its sandboxes running.
        let _ = std::thread::Builder::new()
            .name("sandstorm-fc-reap".into())
            .spawn(move || {
                let _ = child.wait();
            });

        let deadline = Instant::now() + Duration::from_secs(5);
        while UnixStream::connect(&sock).is_err() {
            if Instant::now() > deadline || !alive(pid) {
                self.stop(id);
                bail!("firecracker for {id} did not open its API socket; see its log");
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }

    /// Kill this sandbox's Firecracker, if it is still the process we
    /// started -- a pid on its own could have been reused.
    fn stop(&self, id: &str) {
        let pidf = self.bundle(id).join("fc.pid");
        let sock = self.sock(id);
        if let Some(pid) = std::fs::read_to_string(&pidf)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
        {
            let ours = std::fs::read(format!("/proc/{pid}/cmdline"))
                .map(|c| String::from_utf8_lossy(&c).contains(sock.to_string_lossy().as_ref()))
                .unwrap_or(false);
            if ours {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
                let deadline = Instant::now() + Duration::from_secs(2);
                while alive(pid as u32) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
        std::fs::remove_file(&pidf).ok();
        std::fs::remove_file(&sock).ok();
    }

    fn ensure_net(&self, cfg: &VmConfig) -> Result<()> {
        match &cfg.netns {
            Some(ns) => ensure_tap(ns),
            None => Ok(()),
        }
    }
}

impl Backend for Firecracker {
    /// The tap and its NAT rules depend on nothing but the namespace: every
    /// guest is 10.42.0.2 behind its own tap.
    fn prepare_netns(&self, netns: &str) -> Result<()> {
        ensure_tap(netns)
    }

    /// The guest is on the tap, which a pooled namespace already has; the
    /// veth only carries what the node sends in.
    fn veth_needed_to_start(&self) -> bool {
        false
    }

    /// A paused VM goes on where it stopped. Stopping it and restoring it
    /// from the image instead, as runsc must, assembled the whole diff chain
    /// every time: on bm the parent's return grew from 73 to 285 ms over six
    /// branches of one app64. Its memory now stands on the image's chain, so
    /// that is the base the next checkpoint diffs against; pages it dirties
    /// from here are tracked from the snapshot just taken, and a tracker
    /// that kept older ones too would only make the diff larger.
    fn resume_after_checkpoint(&self, id: &str, image: &Path) -> Result<bool> {
        let chain = chain_of(image)?;
        link_chain(image, &chain, &self.base_dir(id))?;
        if self.tracked(id).is_some() {
            clear_soft_dirty(self.pid(id)?)?;
        }
        api(
            &self.sock(id),
            "PATCH",
            "/vm",
            &json!({ "state": "Resumed" }),
            API_TIMEOUT,
        )
        .with_context(|| format!("resuming {id}"))?;
        Ok(true)
    }

    fn version(&self) -> String {
        Exec::new(&self.bin)
            .arg("--version")
            .output()
            .ok()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string()
            })
            .unwrap_or_default()
    }

    fn template_root(&self, dir: &Path) -> PathBuf {
        dir.join("rootfs.ext4")
    }

    fn prepare(&self, _: &RuntimeConfig, req: &BuildRequest<'_>) -> Result<PathBuf> {
        if req.volume.is_some() {
            bail!("volumes are not supported on the firecracker backend yet");
        }
        let disk = self.disk_dir(req.id);
        std::fs::create_dir_all(&disk)?;
        let rootfs = disk.join("rootfs.ext4");
        if !rootfs.exists() {
            match req.restore_from {
                Some(arc) => bundle::unpack_into(arc, &disk)
                    .with_context(|| format!("restoring {} from {}", req.id, arc.display()))?,
                None => reflink(req.template_rootfs, &rootfs).with_context(|| {
                    format!("giving {} its own copy of the template's disk", req.id)
                })?,
            }
        } else if req.restore_from.is_some() {
            bail!("{} already has a disk; it cannot be restored over", req.id);
        }

        let mem_mib = req.spec.memory_max_mib.unwrap_or(DEFAULT_MEM_MIB);
        let cfg = VmConfig {
            netns: req
                .netns
                .map(|p| p.rsplit('/').next().unwrap_or(p).to_string()),
            sandbox_ip: req.sandbox_ip,
            mem_mib,
            vcpus: self.vcpus,
            boot_args: boot_args(req.spec, req.port, req.netns.is_some(), self.serial_console)?,
        };
        let b = self.bundle(req.id);
        std::fs::create_dir_all(&b)?;
        std::fs::write(b.join("config.json"), serde_json::to_vec_pretty(&cfg)?)?;
        Ok(b)
    }

    fn layer(&self, _: &RuntimeConfig, id: &str) -> PathBuf {
        self.disk_dir(id)
    }

    fn unmount(&self, _: &RuntimeConfig, _: &str) -> Result<()> {
        Ok(())
    }

    fn teardown(&self, cfg: &RuntimeConfig, id: &str) -> Result<()> {
        self.stop(id);
        std::fs::remove_dir_all(cfg.work_dir.join("layers").join(id)).ok();
        std::fs::remove_dir_all(self.bundle(id)).ok();
        Ok(())
    }

    fn boot(&self, id: &str, _bundle: &Path) -> Result<()> {
        let cfg = self.config(id)?;
        self.ensure_net(&cfg)?;
        // A cold boot has no image under it, so its first checkpoint is full,
        // and KVM tracks what it writes after that.
        std::fs::remove_dir_all(self.base_dir(id)).ok();
        std::fs::remove_file(self.bundle(id).join(SOFT_DIRTY)).ok();
        self.spawn(id, &cfg)?;
        let sock = self.sock(id);
        let mut calls = vec![
            (
                "/boot-source",
                json!({ "kernel_image_path": self.kernel, "boot_args": cfg.boot_args }),
            ),
            (
                "/drives/rootfs",
                json!({
                    "drive_id": "rootfs",
                    "path_on_host": "rootfs.ext4",
                    "is_root_device": true,
                    "is_read_only": false,
                }),
            ),
            (
                "/machine-config",
                json!({
                    "vcpu_count": cfg.vcpus,
                    "mem_size_mib": cfg.mem_mib,
                    // Without it there are no diff snapshots of this VM.
                    "track_dirty_pages": true,
                }),
            ),
        ];
        if cfg.netns.is_some() {
            calls.push((
                "/network-interfaces/eth0",
                json!({ "iface_id": "eth0", "host_dev_name": TAP, "guest_mac": GUEST_MAC }),
            ));
        }
        calls.push(("/entropy", json!({})));
        calls.push(("/actions", json!({ "action_type": "InstanceStart" })));
        let r = calls
            .iter()
            .try_for_each(|(path, body)| api(&sock, "PUT", path, body, API_TIMEOUT).map(drop));
        if let Err(e) = r {
            self.stop(id);
            return Err(e).with_context(|| format!("booting {id}"));
        }
        Ok(())
    }

    fn restore(&self, id: &str, _bundle: &Path, image: &Path) -> Result<()> {
        let cfg = self.config(id)?;
        self.ensure_net(&cfg)?;

        // This VM's disk becomes the one taken with the memory it is about
        // to load. A reflink, so free; tmp and rename, so a failure leaves
        // the old disk rather than half of one.
        let disk = self.disk_dir(id);
        std::fs::create_dir_all(&disk)?;
        let tmp = disk.join("rootfs.ext4.tmp");
        let _ = std::fs::remove_file(&tmp);
        reflink(&image.join("rootfs.ext4"), &tmp)
            .with_context(|| format!("taking {id}'s disk from {}", image.display()))?;
        std::fs::rename(&tmp, disk.join("rootfs.ext4"))?;

        let chain = chain_of(image)?;
        let mem = assemble(image, &chain)?;
        link_chain(image, &chain, &self.base_dir(id))?;

        let marker = self.bundle(id).join(SOFT_DIRTY);
        std::fs::remove_file(&marker).ok();
        self.spawn(id, &cfg)?;
        let r = self.pid(id).and_then(|pid| {
            load(
                &self.sock(id),
                &image.join("vmstate"),
                &mem,
                &disk.join("rootfs.ext4"),
                || track_writes(pid, &mem, &marker),
            )
        });
        if let Err(e) = r {
            // Never left half-loaded, and never resumed on a disk that was
            // not re-pointed.
            self.stop(id);
            return Err(e).with_context(|| format!("restoring {id} from {}", image.display()));
        }
        if let Some(ip) = cfg.sandbox_ip {
            let _ = open_then_sync(ip, net::allow, sync_clock);
        }
        Ok(())
    }

    fn validate(&self, image: &Path) -> Result<()> {
        for name in ["vmstate", "rootfs.ext4"] {
            let len = std::fs::metadata(image.join(name))
                .with_context(|| format!("no {name} in {}", image.display()))?
                .len();
            if len == 0 {
                bail!("{name} in {} is empty", image.display());
            }
        }
        chain_of(image).map(drop)
    }

    fn checkpoint(&self, id: &str, image: &Path, _compress: bool) -> Result<()> {
        let sock = self.sock(id);
        api(
            &sock,
            "PATCH",
            "/vm",
            &json!({ "state": "Paused" }),
            API_TIMEOUT,
        )
        .with_context(|| format!("pausing {id}"))?;

        let r = (|| -> Result<()> {
            let base = self.base_dir(id);
            let have = chain_of(&base).unwrap_or_default();
            let (kind, next) = if !have.is_empty() && have.len() < MAX_CHAIN {
                link_chain(&base, &have, image)?;
                ("Diff", have.len())
            } else {
                ("Full", 0)
            };
            let mem = image.join(format!("mem.{next}"));
            match self.tracked(id).filter(|_| kind == "Diff") {
                Some(file) => {
                    save_vmstate(&sock, &image.join("vmstate"))?;
                    let len = std::fs::metadata(base.join("mem.0"))?.len();
                    let pages = write_dirty(self.pid(id)?, &file, &mem, len)?;
                    tracing::debug!(%id, pages, "soft-dirty diff");
                }
                None => {
                    api(
                        &sock,
                        "PUT",
                        "/snapshot/create",
                        &json!({
                            "snapshot_type": kind,
                            "snapshot_path": image.join("vmstate"),
                            "mem_file_path": mem,
                        }),
                        SNAPSHOT_TIMEOUT,
                    )?;
                }
            }
            // Paused, so the disk is exactly what the memory expects.
            reflink(
                &self.disk_dir(id).join("rootfs.ext4"),
                &image.join("rootfs.ext4"),
            )?;
            tracing::info!(%id, kind, links = next + 1, "firecracker snapshot");
            Ok(())
        })();
        match r {
            // Left paused: every caller either removes it or, for a branch,
            // resumes it (`resume_after_checkpoint`).
            Ok(()) => Ok(()),
            Err(e) => {
                // Leave it running rather than paused for ever.
                let _ = api(
                    &sock,
                    "PATCH",
                    "/vm",
                    &json!({ "state": "Resumed" }),
                    API_TIMEOUT,
                );
                Err(e).with_context(|| format!("snapshotting {id}"))
            }
        }
    }

    fn remove(&self, id: &str) {
        self.stop(id);
    }

    fn kill(&self, id: &str) {
        self.stop(id);
    }

    fn exec(&self, id: &str, args: &[String], timeout: Duration) -> Result<(String, String, i64)> {
        if args.is_empty() {
            bail!("exec needs a command to run");
        }
        let ip = self
            .config(id)?
            .sandbox_ip
            .with_context(|| format!("{id} has no network, so its agent cannot be reached"))?;
        let resp = agent(
            ip,
            &json!({ "action": "exec", "args": args, "timeout": timeout.as_secs().max(1) }),
            timeout + Duration::from_secs(5),
        );
        let resp = match resp {
            Ok(r) => r,
            Err(e) if is_timeout(&e) => {
                return Ok((
                    String::new(),
                    format!("\nsandstorm: killed after {}s\n", timeout.as_secs()),
                    124,
                ))
            }
            Err(e) => return Err(e).with_context(|| format!("exec in {id}")),
        };
        if let Some(err) = resp.get("error").and_then(Value::as_str) {
            bail!("the agent in {id} refused the exec: {err}");
        }
        let s = |k: &str| {
            resp.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let code = resp.get("exit_code").and_then(Value::as_i64).unwrap_or(-1);
        Ok((s("stdout"), s("stderr"), code))
    }
}

/// Whether this host's KVM is PVM, which runs guests in software on a VM with
/// no virtualisation of its own. Every trap to the host costs far more there,
/// and a serial console is nothing but traps: on bm a cold start lost 150-200
/// ms to it and to what these boot options skip, 740 / 897 ms for web and a
/// 64 MiB app against 903 / 1,103. On real KVM the same change gained nothing
/// and the console is a sandbox's only log, so it stays there, and
/// `--fc-serial-console` brings it back here to see why a guest will not start.
pub fn pvm_host() -> bool {
    Path::new("/sys/module/kvm_pvm").exists()
}

/// The kernel command line. The template's program goes in as hex JSON,
/// which `sandstorm-agent.py` starts beside itself.
fn boot_args(spec: &TemplateSpec, port: u16, net: bool, serial: bool) -> Result<String> {
    let args: Vec<String> = spec
        .args
        .iter()
        .map(|a| a.replace("{{PORT}}", &port.to_string()))
        .collect();
    let app = json!({ "args": args, "env": spec.env, "cwd": spec.cwd });
    // The i8042 options are Firecracker's own advice: it emulates no PS/2
    // controller, and a kernel that probes for one waits it out. On bm that
    // was 0.49 s of a 0.77 s boot to init; with them, 0.29 s.
    let mut s = String::from(if serial {
        "console=ttyS0 "
    } else {
        "no_timer_check noreplace-smp audit=0 raid=noautodetect "
    });
    s.push_str(
        "quiet reboot=k panic=1 pci=off clocksource=kvm-clock \
         i8042.noaux i8042.nomux i8042.nopnp i8042.dumbkbd init=/sandstorm-init.sh",
    );
    if net {
        s.push_str(&format!(
            " ip={GUEST_IP}::10.42.0.1:255.255.255.252::eth0:off"
        ));
    }
    s.push_str(" sandstorm.app=");
    s.push_str(&hex::encode(serde_json::to_vec(&app)?));
    if s.len() >= CMDLINE_MAX {
        bail!(
            "the template's args and env make a {}-byte kernel command line; \
             the limit is {CMDLINE_MAX}",
            s.len()
        );
    }
    Ok(s)
}

/// Load a snapshot without running it, point the disk at this sandbox's
/// own, then run it -- the order PR #321 established. The relative disk path
/// already resolves to this sandbox's copy; the re-point is the second
/// guarantee, and the guest executes nothing until both have happened.
/// `before_resume` runs last, with the guest still stopped.
///
/// Without KVM dirty logging: with it, every page the guest touched first
/// faulted in on its own, and the first answer of a warm `app64`, which
/// reads 64 MiB, took 40 ms longer on bm (103 against 65 ms). What a diff
/// needs is tracked by `track_writes` instead.
fn load(
    sock: &Path,
    vmstate: &Path,
    mem: &Path,
    disk: &Path,
    before_resume: impl FnOnce() -> Result<()>,
) -> Result<()> {
    api(
        sock,
        "PUT",
        "/snapshot/load",
        &json!({
            "snapshot_path": vmstate,
            "mem_backend": { "backend_type": "File", "backend_path": mem },
            "enable_diff_snapshots": false,
            "resume_vm": false,
        }),
        SNAPSHOT_TIMEOUT,
    )
    .context("loading the snapshot")?;
    api(
        sock,
        "PATCH",
        "/drives/rootfs",
        &json!({ "drive_id": "rootfs", "path_on_host": disk }),
        API_TIMEOUT,
    )
    .context("pointing the disk at this sandbox's own")?;
    before_resume()?;
    api(
        sock,
        "PATCH",
        "/vm",
        &json!({ "state": "Resumed" }),
        API_TIMEOUT,
    )
    .context("resuming")?;
    Ok(())
}

/// A file as `/proc/<pid>/maps` names it: device as `major:minor` in hex,
/// and inode.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct MappedFile {
    dev: String,
    inode: u64,
}

impl MappedFile {
    #[cfg(target_os = "linux")]
    fn of(path: &Path) -> Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
        let dev = m.dev();
        Ok(MappedFile {
            dev: format!("{:02x}:{:02x}", libc::major(dev), libc::minor(dev)),
            inode: m.ino(),
        })
    }

    /// The ranges of `maps` that map this file: start, end, and the
    /// file offset at start. The offset is also where those pages sit in
    /// a memory image, which is laid out as the file is.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn ranges(&self, maps: &str) -> Vec<(u64, u64, u64)> {
        maps.lines()
            .filter_map(|l| {
                let mut f = l.split_whitespace();
                let (range, _perms, off, dev, inode) =
                    (f.next()?, f.next()?, f.next()?, f.next()?, f.next()?);
                if dev != self.dev || inode.parse::<u64>().ok()? != self.inode {
                    return None;
                }
                let (a, b) = range.split_once('-')?;
                Some((
                    u64::from_str_radix(a, 16).ok()?,
                    u64::from_str_radix(b, 16).ok()?,
                    u64::from_str_radix(off, 16).ok()?,
                ))
            })
            .collect()
    }
}

/// Track what a restored VM writes from here: record the memory file it
/// maps, and clear its soft-dirty bits. A page the guest -- or Firecracker
/// for it, as a virtio device does -- writes after this is marked in
/// `/proc/<pid>/pagemap`, and `write_dirty` takes exactly those.
///
/// CubeSandbox snapshots the same way. KVM's own dirty log, which a diff
/// otherwise needs, costs every first touch of a page for as long as the VM
/// runs; this costs one pass over the page table, at the snapshot.
fn track_writes(pid: u32, mem: &Path, marker: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let file = MappedFile::of(mem)?;
        clear_soft_dirty(pid)?;
        std::fs::write(marker, serde_json::to_vec(&file)?)?;
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (pid, mem, marker);
        Ok(())
    }
}

fn clear_soft_dirty(pid: u32) -> Result<()> {
    std::fs::write(format!("/proc/{pid}/clear_refs"), "4")
        .with_context(|| format!("clearing soft-dirty bits of {pid}"))
}

/// Have Firecracker write the vmstate and none of the memory.
///
/// It has no call for that, but it writes the vmstate first, then opens
/// the memory file and sizes it: `/dev/null` cannot be sized, so it stops
/// there, with the vmstate whole and nothing else done. Its one later step,
/// marking virtio queues in its own dirty bitmap, is for KVM-tracked diffs,
/// which these are not.
fn save_vmstate(sock: &Path, vmstate: &Path) -> Result<()> {
    let _ = std::fs::remove_file(vmstate);
    let r = api(
        sock,
        "PUT",
        "/snapshot/create",
        &json!({
            "snapshot_type": "Full",
            "snapshot_path": vmstate,
            "mem_file_path": "/dev/null",
        }),
        SNAPSHOT_TIMEOUT,
    );
    match r {
        Ok(_) => {}
        Err(e) if format!("{e:#}").contains("memory backing file") => {}
        Err(e) => return Err(e).context("saving the vmstate"),
    }
    let len = std::fs::metadata(vmstate).map_or(0, |m| m.len());
    if len == 0 {
        bail!("firecracker wrote no vmstate to {}", vmstate.display());
    }
    Ok(())
}

/// Bit 55 of a pagemap entry: written since the last clear.
#[cfg(target_os = "linux")]
const PM_SOFT_DIRTY: u64 = 1 << 55;

/// Write the pages of `file` that a paused VM has written since its bits
/// were last cleared into a sparse diff of `len` bytes at `out`, in the
/// layout `apply_diff` reads. Returns how many pages it wrote.
#[cfg(target_os = "linux")]
fn write_dirty(pid: u32, file: &MappedFile, out: &Path, len: u64) -> Result<u64> {
    use std::os::unix::fs::FileExt;
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    let maps = std::fs::read_to_string(format!("/proc/{pid}/maps"))?;
    let ranges = file.ranges(&maps);
    if ranges.is_empty() {
        bail!("the guest's memory is not mapped in {pid}");
    }
    let pagemap = std::fs::File::open(format!("/proc/{pid}/pagemap"))?;
    let memf = std::fs::File::open(format!("/proc/{pid}/mem"))?;
    let o = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(out)
        .with_context(|| format!("creating {}", out.display()))?;
    o.set_len(len)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut written = 0;
    for (start, end, off) in ranges {
        let n = ((end - start) / page) as usize;
        let mut entries = vec![0u8; n * 8];
        pagemap.read_exact_at(&mut entries, start / page * 8)?;
        let dirty = |i: usize| {
            u64::from_le_bytes(entries[i * 8..i * 8 + 8].try_into().unwrap()) & PM_SOFT_DIRTY != 0
        };
        let mut i = 0;
        while i < n {
            if !dirty(i) {
                i += 1;
                continue;
            }
            let run = i;
            while i < n && dirty(i) {
                i += 1;
            }
            let (mut at, stop) = (run as u64 * page, i as u64 * page);
            if off + stop > len {
                bail!("the guest's memory runs past its {len}-byte image");
            }
            while at < stop {
                let take = ((stop - at) as usize).min(buf.len());
                memf.read_exact_at(&mut buf[..take], start + at)?;
                o.write_all_at(&buf[..take], off + at)?;
                at += take as u64;
            }
            written += (i - run) as u64;
        }
    }
    Ok(written)
}

#[cfg(not(target_os = "linux"))]
fn write_dirty(_: u32, _: &MappedFile, _: &Path, _: u64) -> Result<u64> {
    bail!("soft-dirty tracking is Linux's")
}

/// The memory files of an image, in chain order: `mem.0`, `mem.1`, ...
/// A gap means a broken image, which is refused rather than loaded short.
fn chain_of(dir: &Path) -> Result<Vec<String>> {
    let mut idx: Vec<usize> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_prefix("mem.")?
                .parse::<usize>()
                .ok()
        })
        .collect();
    idx.sort_unstable();
    if idx.is_empty() {
        bail!("{} has no memory image", dir.display());
    }
    if idx.iter().enumerate().any(|(i, &n)| i != n) {
        bail!("{} has a gap in its memory chain: {idx:?}", dir.display());
    }
    Ok(idx.into_iter().map(|i| format!("mem.{i}")).collect())
}

/// Hard-link a chain from one directory into another, replacing whatever
/// chain was there. Links, not copies or pointers: each image stays whole
/// on its own, so dropping a tag cannot break one derived from it.
fn link_chain(from: &Path, chain: &[String], to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(to)?.flatten() {
        if e.file_name().to_string_lossy().starts_with("mem.") {
            std::fs::remove_file(e.path())?;
        }
    }
    for f in chain {
        std::fs::hard_link(from.join(f), to.join(f))
            .with_context(|| format!("linking {f} into {}", to.display()))?;
    }
    Ok(())
}

/// The one memory file to load: `mem.0` itself, or the chain applied to a
/// reflink of it, cached in the image so every later fork maps the same
/// inode. The cache is a dot file, which tag transfer leaves out.
fn assemble(image: &Path, chain: &[String]) -> Result<PathBuf> {
    if chain.len() == 1 {
        return Ok(image.join(&chain[0]));
    }
    let out = image.join(".mem.assembled");
    if out.exists() {
        return Ok(out);
    }
    let tmp = image.join(format!(".mem.assembled.{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    reflink(&image.join(&chain[0]), &tmp)?;
    for f in &chain[1..] {
        apply_diff(&image.join(f), &tmp).with_context(|| format!("applying {f}"))?;
    }
    std::fs::rename(&tmp, &out)?;
    Ok(out)
}

/// Write each data range of a sparse diff over the same range of `base`.
/// A Firecracker Diff snapshot is the guest's whole address space with only
/// the dirtied pages present, so its holes are exactly what did not change.
#[cfg(target_os = "linux")]
fn apply_diff(diff: &Path, base: &Path) -> Result<()> {
    use std::io::{Seek, SeekFrom};
    use std::os::unix::io::AsRawFd;
    let mut d = std::fs::File::open(diff)?;
    let mut b = std::fs::OpenOptions::new().write(true).open(base)?;
    let len = d.metadata()?.len();
    if b.metadata()?.len() != len {
        bail!("{} and {} differ in size", diff.display(), base.display());
    }
    let mut buf = vec![0u8; 1 << 20];
    let mut at: i64 = 0;
    while (at as u64) < len {
        let start = unsafe { libc::lseek(d.as_raw_fd(), at, libc::SEEK_DATA) };
        if start < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ENXIO) {
                break; // no data past `at`
            }
            return Err(e.into());
        }
        let end = unsafe { libc::lseek(d.as_raw_fd(), start, libc::SEEK_HOLE) };
        if end < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        d.seek(SeekFrom::Start(start as u64))?;
        b.seek(SeekFrom::Start(start as u64))?;
        let mut left = (end - start) as usize;
        while left > 0 {
            let take = left.min(buf.len());
            let n = d.read(&mut buf[..take])?;
            if n == 0 {
                break;
            }
            b.write_all(&buf[..n])?;
            left -= n;
        }
        at = end;
    }
    b.sync_data()?;
    Ok(())
}

/// Elsewhere there is no SEEK_DATA to trust; fall back to writing every
/// non-zero page, which is what a hole reads as. Only for tests: the
/// backend itself runs on Linux.
#[cfg(not(target_os = "linux"))]
fn apply_diff(diff: &Path, base: &Path) -> Result<()> {
    use std::io::{Seek, SeekFrom};
    let mut d = std::fs::File::open(diff)?;
    let mut b = std::fs::OpenOptions::new().write(true).open(base)?;
    if b.metadata()?.len() != d.metadata()?.len() {
        bail!("{} and {} differ in size", diff.display(), base.display());
    }
    let mut page = vec![0u8; 4096];
    let mut at = 0u64;
    loop {
        let n = d.read(&mut page)?;
        if n == 0 {
            break;
        }
        if page[..n].iter().any(|&x| x != 0) {
            b.seek(SeekFrom::Start(at))?;
            b.write_all(&page[..n])?;
        }
        at += n as u64;
    }
    Ok(())
}

/// A copy-on-write clone, or an error. Never a silent full copy: that is
/// how v0.5 lost its density without anyone noticing (#322).
fn reflink(src: &Path, dst: &Path) -> Result<()> {
    let s = std::fs::File::open(src).with_context(|| format!("opening {}", src.display()))?;
    let d = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)
        .with_context(|| format!("creating {}", dst.display()))?;
    let r = clone_file(&s, &d);
    if r.is_err() {
        drop(d);
        std::fs::remove_file(dst).ok();
    }
    r.with_context(|| format!("reflinking {} to {}", src.display(), dst.display()))
}

/// _IOW(0x94, 9, int). v0.5 had this number wrong (#322) and fell back to
/// copying on every filesystem; a test pins it.
#[cfg(target_os = "linux")]
const FICLONE: libc::c_ulong = 0x4004_9409;

#[cfg(target_os = "linux")]
fn clone_file(src: &std::fs::File, dst: &std::fs::File) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    if unsafe { libc::ioctl(dst.as_raw_fd(), FICLONE as _, src.as_raw_fd()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

/// macOS has clonefile(2) on APFS, but not on an open descriptor; tests
/// get a plain copy so the chain logic can be exercised here.
#[cfg(not(target_os = "linux"))]
fn clone_file(src: &std::fs::File, dst: &std::fs::File) -> Result<()> {
    let (mut s, mut d) = (src, dst);
    std::io::copy(&mut s, &mut d)?;
    Ok(())
}

/// Put the tap and the two NAT rules into the sandbox's namespace. Every
/// step checks first, so this is safe to run again before every boot and
/// restore -- which it must, since Snapshot releases the network.
///
/// The tap is addressed and brought up last, so a tap that is up with its
/// address means everything before it is in place too: one `ip` to find
/// that out, rather than the nine this takes from nothing. A namespace from
/// `net::Pool` arrives that way, which is what the pool is for.
fn ensure_tap(ns: &str) -> Result<()> {
    let up = Exec::new("ip")
        .args(["-n", ns, "-o", "-4", "addr", "show", "dev", TAP, "up"])
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(TAP_CIDR))
        .unwrap_or(false);
    if up {
        return Ok(());
    }
    let ok = |args: &[&str]| {
        Exec::new(args[0])
            .args(&args[1..])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    let run = |what: &str, args: &[&str]| -> Result<()> {
        let out = Exec::new(args[0])
            .args(&args[1..])
            .output()
            .with_context(|| format!("running {args:?}"))?;
        if !out.status.success() {
            bail!("{what}: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    };
    if !ok(&["ip", "-n", ns, "link", "show", TAP]) {
        run(
            "creating the tap",
            &["ip", "-n", ns, "tuntap", "add", "dev", TAP, "mode", "tap"],
        )?;
    }
    run(
        "forwarding in the namespace",
        &[
            "ip",
            "netns",
            "exec",
            ns,
            "sysctl",
            "-qw",
            "net.ipv4.ip_forward=1",
        ],
    )?;
    // Everything that arrives from outside goes to the guest; what the
    // guest sends leaves as the namespace's own address, so the node's
    // rules for 10.86/16 apply to it unchanged.
    let rules: [&[&str]; 2] = [
        &[
            "PREROUTING",
            "!",
            "-i",
            TAP,
            "-j",
            "DNAT",
            "--to-destination",
            GUEST_IP,
        ],
        &[
            "POSTROUTING",
            "-s",
            GUEST_IP,
            "!",
            "-o",
            TAP,
            "-j",
            "MASQUERADE",
        ],
    ];
    for rule in rules {
        let mut check = vec!["ip", "netns", "exec", ns, "iptables", "-t", "nat", "-C"];
        check.extend_from_slice(rule);
        if !ok(&check) {
            let mut add = check.clone();
            add[7] = "-A";
            run("adding a NAT rule", &add)?;
        }
    }
    run(
        "fixing the tap's MAC",
        &["ip", "-n", ns, "link", "set", TAP, "address", TAP_MAC],
    )?;
    // The guest's MAC is fixed, so its neighbour entry is too. Left to ARP,
    // the first packet for a guest still booting asks, gets no answer, and
    // the kernel asks again only a second later: on bare metal every cold
    // sandbox answered at 1.1 s with its program listening from 0.6 s, the
    // gateway's SYNs queued behind the second ARP.
    run(
        "pinning the guest's address",
        &[
            "ip",
            "-n",
            ns,
            "neigh",
            "replace",
            GUEST_IP,
            "lladdr",
            GUEST_MAC,
            "dev",
            TAP,
            "nud",
            "permanent",
        ],
    )?;
    run(
        "addressing the tap",
        &["ip", "-n", ns, "addr", "replace", TAP_CIDR, "dev", TAP],
    )?;
    run("tap up", &["ip", "-n", ns, "link", "set", TAP, "up"])?;
    Ok(())
}

/// The sandbox's cgroup, created with its memory ceiling. The same path
/// runsc's bundle names, so the node's freeze, accounting and kill work on
/// a VM unchanged.
fn cgroup(id: &str, max_mib: u64) -> Result<PathBuf> {
    let parent = Path::new("/sys/fs/cgroup/sandstorm");
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    // Controllers runsc would otherwise have enabled. Already-enabled ones
    // are no error; a missing one shows up as the memory.max write failing.
    let _ = std::fs::write(parent.join("cgroup.subtree_control"), "+memory +cpu +pids");
    let cg = parent.join(id);
    std::fs::create_dir_all(&cg)?;
    std::fs::write(cg.join("memory.max"), (max_mib << 20).to_string())
        .with_context(|| format!("setting {}/memory.max", cg.display()))?;
    Ok(cg)
}

fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// One request to the guest agent: a JSON line out, a JSON line back.
fn agent(ip: Ipv4Addr, req: &Value, timeout: Duration) -> Result<Value> {
    let addr = SocketAddr::from((ip, AGENT_PORT));
    // Not an io::Error once it leaves here: a connect that times out is an
    // agent that is not there, not a command that ran too long.
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .map_err(|e| anyhow::anyhow!("cannot reach the agent at {addr}: {e}"))?;
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut line = serde_json::to_vec(req)?;
    line.push(b'\n');
    s.write_all(&line)?;
    s.shutdown(std::net::Shutdown::Write)?;
    let mut out = Vec::new();
    s.read_to_end(&mut out)?;
    let first = out.split(|&b| b == b'\n').next().unwrap_or(&[]);
    serde_json::from_slice(first).context("the agent's answer is not JSON")
}

fn is_timeout(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>().is_some_and(|io| {
        matches!(
            io.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        )
    })
}

/// Let a restored guest be reached, then set its clock.
///
/// A sandbox its node put to disk to make room is still refused by a REJECT in
/// the host's OUTPUT, and that rule stays until `set_tier` has finished coming
/// back. The agent is reached through OUTPUT too, so the rule stood in the way
/// of the restore's own clock: on PVM, 960 refused connects, the whole 2 s of
/// `sync_clock`, and a guest not told the time. The VM is running by now, so
/// "not up yet, refuse" no longer holds. Whatever is wrong with the rule, the
/// clock is still worth trying.
fn open_then_sync(
    ip: Ipv4Addr,
    allow: impl FnOnce(Ipv4Addr) -> Result<()>,
    sync: impl FnOnce(Ipv4Addr),
) -> Result<()> {
    let opened = allow(ip);
    if let Err(e) = &opened {
        tracing::warn!(%ip, error = %format!("{e:#}"), "could not let a restored guest be reached");
    }
    sync(ip);
    opened
}

/// A restored guest's clock is wherever it was when the image was taken.
/// Best effort and bounded: a sandbox whose clock could not be set is still
/// better served than one that was not restored.
///
/// One `set_time` to the agent, which sets it in-process. An agent from
/// before that action answers with an error, and gets `date -s` through
/// `exec` as before, which forks in the guest: 15 ms of a 30 ms warm
/// restore on bm.
fn sync_clock(ip: Ipv4Addr) {
    let now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64())
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let req = json!({ "action": "set_time", "unix": now() });
        match agent(ip, &req, Duration::from_millis(500)) {
            Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => return,
            Ok(_) => {
                let req = json!({
                    "action": "exec",
                    "args": ["date", "-s", format!("@{}", now() as u64)],
                    "timeout": 2,
                });
                if let Err(e) = agent(ip, &req, Duration::from_secs(3)) {
                    tracing::warn!(%ip, error = %e, "could not set a restored guest's clock");
                }
                return;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(2)),
        }
    }
    tracing::warn!(%ip, "a restored guest's agent did not answer; its clock is stale");
}

/// `read`, carrying on through a signal. The kernel never restarts a read on
/// a socket with a timeout set, even for a handler with `SA_RESTART`, so a
/// signal landing on this thread would otherwise read as a failed call.
fn read_past_signals(s: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    loop {
        match s.read(buf) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            out => return out,
        }
    }
}

/// One call to Firecracker's API over its Unix socket. Returns the body;
/// any status outside 2xx is an error carrying Firecracker's own message.
fn api(sock: &Path, method: &str, path: &str, body: &Value, timeout: Duration) -> Result<String> {
    let mut s =
        UnixStream::connect(sock).with_context(|| format!("connecting to {}", sock.display()))?;
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(timeout))?;
    let body = serde_json::to_vec(body)?;
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes())?;
    s.write_all(&body)?;

    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let (head_end, want) = loop {
        let n = read_past_signals(&mut s, &mut chunk)
            .with_context(|| format!("{method} {path}: no answer"))?;
        if n == 0 {
            bail!("{method} {path}: the connection closed before an answer");
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..i]).to_string();
            let len = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            break (i + 4, len);
        }
    };
    while buf.len() < head_end + want {
        let n = read_past_signals(&mut s, &mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let status_line =
        String::from_utf8_lossy(&buf[..buf.iter().position(|&b| b == b'\r').unwrap_or(0)])
            .to_string();
    let code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .with_context(|| format!("{method} {path}: unreadable status {status_line:?}"))?;
    let resp = String::from_utf8_lossy(&buf[head_end..]).to_string();
    if !(200..300).contains(&code) {
        bail!("{method} {path} -> {code}: {}", resp.trim());
    }
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    /// A stand-in for Firecracker's API: records each request line and
    /// answers with the next status, or 204.
    #[test]
    fn a_restored_guest_is_let_through_before_its_clock_is_set() {
        let ip = Ipv4Addr::new(10, 88, 0, 2);
        let order = std::cell::RefCell::new(Vec::new());
        open_then_sync(
            ip,
            |_| {
                order.borrow_mut().push("allow");
                Ok(())
            },
            |_| order.borrow_mut().push("sync"),
        )
        .unwrap();
        assert_eq!(*order.borrow(), ["allow", "sync"]);
    }

    #[test]
    fn a_rule_that_will_not_go_does_not_cost_the_clock_its_try() {
        let synced = std::cell::Cell::new(false);
        let r = open_then_sync(
            Ipv4Addr::new(10, 88, 0, 2),
            |_| bail!("iptables is gone"),
            |_| synced.set(true),
        );
        assert!(r.is_err());
        assert!(synced.get());
    }

    fn fake_api(dir: &Path, statuses: Vec<u16>) -> (PathBuf, Arc<Mutex<Vec<String>>>) {
        let sock = dir.join("fc.sock");
        let l = UnixListener::bind(&sock).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let rec = seen.clone();
        std::thread::spawn(move || {
            let mut statuses = statuses.into_iter();
            for conn in l.incoming() {
                let mut c = conn.unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                // Read the head, then the body by its length.
                let (end, len) = loop {
                    let n = c.read(&mut chunk).unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let h = String::from_utf8_lossy(&buf[..i]).to_string();
                        let len = h
                            .lines()
                            .find_map(|l| l.strip_prefix("Content-Length: "))
                            .and_then(|v| v.parse::<usize>().ok())
                            .unwrap_or(0);
                        break (i + 4, len);
                    }
                };
                while buf.len() < end + len {
                    let n = c.read(&mut chunk).unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                }
                let line =
                    String::from_utf8_lossy(&buf[..buf.iter().position(|&b| b == b'\r').unwrap()])
                        .to_string();
                let body = String::from_utf8_lossy(&buf[end..end + len]).to_string();
                rec.lock().unwrap().push(format!("{line} {body}"));
                let code = statuses.next().unwrap_or(204);
                let msg = if code >= 300 {
                    r#"{"fault_message":"no"}"#
                } else {
                    ""
                };
                write!(
                    c,
                    "HTTP/1.1 {code} X\r\nContent-Length: {}\r\n\r\n{msg}",
                    msg.len()
                )
                .unwrap();
            }
        });
        (sock, seen)
    }

    /// A signal landing on the thread waiting for the answer is not the
    /// answer: this failed a load that Firecracker had not refused.
    #[test]
    fn a_signal_does_not_end_a_call() {
        use std::os::unix::thread::JoinHandleExt;

        extern "C" fn nothing(_: libc::c_int) {}
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("fc.sock");
        let l = UnixListener::bind(&sock).unwrap();
        let server = std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            c.read_exact(&mut [0u8; 1]).unwrap();
            std::thread::sleep(Duration::from_millis(100));
            c.write_all(b"HTTP/1.1 204 X\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });

        // SAFETY: `nothing` does nothing, so it is safe in a signal handler.
        unsafe {
            libc::signal(libc::SIGUSR2, nothing as *const () as libc::sighandler_t);
        }
        let client =
            std::thread::spawn(move || api(&sock, "PATCH", "/vm", &json!({}), API_TIMEOUT).err());
        for _ in 0..10 {
            std::thread::sleep(Duration::from_millis(10));
            // SAFETY: the thread is joined only below, so its id is live.
            unsafe { libc::pthread_kill(client.as_pthread_t(), libc::SIGUSR2) };
        }
        let failed = client.join().unwrap();
        assert!(failed.is_none(), "{failed:?}");
        server.join().unwrap();
    }

    #[test]
    fn the_api_client_reads_status_and_body() {
        let dir = TempDir::new().unwrap();
        let (sock, seen) = fake_api(dir.path(), vec![204, 400]);
        api(&sock, "PUT", "/actions", &json!({"a": 1}), API_TIMEOUT).unwrap();
        let e = api(&sock, "PATCH", "/vm", &json!({}), API_TIMEOUT).unwrap_err();
        assert!(
            e.to_string().contains("400") && e.to_string().contains("no"),
            "{e}"
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0], r#"PUT /actions HTTP/1.1 {"a":1}"#);
    }

    /// The order that keeps a fork off its parent's disk: nothing runs
    /// until the drive has been re-pointed, and a failed re-point never
    /// resumes.
    #[test]
    fn a_load_repoints_the_disk_before_it_resumes() {
        let dir = TempDir::new().unwrap();
        let (sock, seen) = fake_api(dir.path(), vec![]);
        load(
            &sock,
            Path::new("/i/vmstate"),
            Path::new("/i/mem.0"),
            Path::new("/d/rootfs.ext4"),
            || Ok(()),
        )
        .unwrap();
        let seen = seen.lock().unwrap();
        assert!(seen[0].starts_with("PUT /snapshot/load"));
        assert!(seen[0].contains(r#""resume_vm":false"#));
        assert!(seen[0].contains(r#""backend_type":"File""#));
        assert!(seen[0].contains(r#""enable_diff_snapshots":false"#));
        assert!(seen[1].starts_with("PATCH /drives/rootfs"));
        assert!(seen[1].contains("/d/rootfs.ext4"));
        assert_eq!(seen[2], r#"PATCH /vm HTTP/1.1 {"state":"Resumed"}"#);
        assert_eq!(seen.len(), 3);
    }

    #[test]
    fn the_guests_memory_is_found_by_device_and_inode() {
        let f = MappedFile {
            dev: "fd:01".into(),
            inode: 42,
        };
        let maps = "\
7f0000000000-7f0010000000 rw-p 00000000 fd:01 42      /x/.mem.assembled (deleted)
7f0010000000-7f0020000000 rw-p 10000000 fd:01 42      /x/.mem.assembled (deleted)
7f0020000000-7f0021000000 rw-p 00000000 fd:02 42      /y/mem.0
7f0030000000-7f0031000000 rw-p 00000000 00:00 0
";
        assert_eq!(
            f.ranges(maps),
            [
                (0x7f00_0000_0000, 0x7f00_1000_0000, 0),
                (0x7f00_1000_0000, 0x7f00_2000_0000, 0x1000_0000)
            ]
        );
    }

    #[test]
    fn a_failed_repoint_never_resumes() {
        let dir = TempDir::new().unwrap();
        let (sock, seen) = fake_api(dir.path(), vec![204, 400]);
        assert!(load(
            &sock,
            Path::new("/v"),
            Path::new("/m"),
            Path::new("/d"),
            || Ok(())
        )
        .is_err());
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(!seen.iter().any(|l| l.contains("Resumed")));
    }

    /// Whether this filesystem can reflink. CI's ext4 cannot, and the
    /// backend refuses to copy instead, so tests that need it say so and
    /// stop rather than fail.
    fn reflinks(dir: &Path) -> bool {
        let (a, b) = (dir.join(".probe-a"), dir.join(".probe-b"));
        std::fs::write(&a, "x").unwrap();
        let ok = reflink(&a, &b).is_ok();
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
        if !ok {
            eprintln!("skipped: {} cannot reflink", dir.display());
        }
        ok
    }

    fn spec(args: &[&str], env: &[&str]) -> TemplateSpec {
        serde_json::from_value(json!({ "args": args, "env": env })).unwrap()
    }

    #[test]
    fn boot_args_carry_the_program_and_the_fixed_guest_address() {
        let a = boot_args(
            &spec(&["python3", "-m", "http.server", "{{PORT}}"], &["A=1"]),
            8000,
            true,
            true,
        )
        .unwrap();
        assert!(a.contains("init=/sandstorm-init.sh"));
        assert!(a.contains("ip=10.42.0.2::10.42.0.1:255.255.255.252::eth0:off"));
        // Without these the guest kernel waits for a PS/2 controller that
        // Firecracker does not emulate: half a second of every cold boot.
        for opt in ["i8042.noaux", "i8042.nomux", "i8042.nopnp", "i8042.dumbkbd"] {
            assert!(a.split(' ').any(|w| w == opt), "{opt} missing from {a}");
        }
        let hex = a.rsplit("sandstorm.app=").next().unwrap();
        let app: Value = serde_json::from_slice(&hex::decode(hex).unwrap()).unwrap();
        assert_eq!(app["args"][3], "8000", "{{PORT}} must be substituted");
        assert_eq!(app["env"][0], "A=1");
        assert!(!boot_args(&spec(&["x"], &[]), 1, false, true)
            .unwrap()
            .contains("ip="));
    }

    #[test]
    fn a_guest_without_a_serial_console_boots_without_what_it_costs() {
        let on = boot_args(&spec(&["x"], &[]), 1, true, true).unwrap();
        assert!(on.contains("console=ttyS0") && !on.contains("noreplace-smp"));
        let off = boot_args(&spec(&["x"], &[]), 1, true, false).unwrap();
        assert!(!off.contains("console=") && off.contains("no_timer_check noreplace-smp"));
    }

    #[test]
    fn a_command_line_past_the_kernels_limit_is_refused() {
        let big = "x".repeat(1500);
        let e = boot_args(&spec(&["sh"], &[&big]), 1, true, true).unwrap_err();
        assert!(e.to_string().contains("2048"), "{e}");
    }

    #[test]
    fn a_chain_is_read_in_order_and_a_gap_is_refused() {
        let dir = TempDir::new().unwrap();
        for f in ["mem.0", "mem.2", "mem.1", "vmstate", ".mem.assembled"] {
            std::fs::write(dir.path().join(f), "").unwrap();
        }
        assert_eq!(chain_of(dir.path()).unwrap(), ["mem.0", "mem.1", "mem.2"]);
        std::fs::remove_file(dir.path().join("mem.1")).unwrap();
        assert!(chain_of(dir.path()).is_err());
    }

    /// A file of `len` bytes holding only `data`, the rest holes -- the shape
    /// of a Firecracker Diff snapshot.
    fn sparse(path: &Path, len: usize, data: &[(usize, Vec<u8>)]) {
        use std::io::{Seek, SeekFrom};
        let mut f = std::fs::File::create(path).unwrap();
        f.set_len(len as u64).unwrap();
        for (at, bytes) in data {
            f.seek(SeekFrom::Start(*at as u64)).unwrap();
            f.write_all(bytes).unwrap();
        }
    }

    /// No reflink needed, so this runs on CI's ext4: the SEEK_DATA walk must
    /// write a diff's data and leave the base alone under its holes --
    /// including a page of real zeros, which a hole-unaware copy would also
    /// write but a zero-scanning one would skip.
    #[test]
    fn a_diff_writes_its_data_and_nothing_under_its_holes() {
        const P: usize = 4096;
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("base");
        std::fs::write(&base, [vec![7u8; P], vec![7u8; P], vec![7u8; P]].concat()).unwrap();
        let diff = dir.path().join("diff");
        sparse(&diff, 3 * P, &[(0, vec![9u8; P]), (2 * P, vec![0u8; P])]);
        apply_diff(&diff, &base).unwrap();
        let got = std::fs::read(&base).unwrap();
        assert_eq!(&got[..P], &vec![9u8; P][..]);
        assert_eq!(
            &got[P..2 * P],
            &vec![7u8; P][..],
            "a hole must not be written"
        );
        #[cfg(target_os = "linux")]
        assert_eq!(
            &got[2 * P..],
            &vec![0u8; P][..],
            "zeros that are data are data"
        );
        let short = dir.path().join("short");
        sparse(&short, P, &[]);
        assert!(apply_diff(&short, &base).is_err());
    }

    /// Pages present in a later diff win; pages absent from it keep what
    /// the earlier links said.
    #[test]
    fn assembling_applies_each_diff_over_the_one_before() {
        const P: usize = 4096;
        let dir = TempDir::new().unwrap();
        let img = dir.path();
        if !reflinks(img) {
            return;
        }
        let page = |n: u8| vec![n; P];
        // mem.0: pages 1 1 1 1
        std::fs::write(
            img.join("mem.0"),
            [page(1), page(1), page(1), page(1)].concat(),
        )
        .unwrap();
        // mem.1: page 1 is 2, the rest are holes
        sparse(&img.join("mem.1"), 4 * P, &[(P, page(2))]);
        // mem.2: page 3 is 3
        sparse(&img.join("mem.2"), 4 * P, &[(3 * P, page(3))]);

        let chain = chain_of(img).unwrap();
        let out = assemble(img, &chain).unwrap();
        assert_eq!(out, img.join(".mem.assembled"));
        let got = std::fs::read(&out).unwrap();
        assert_eq!(got, [page(1), page(2), page(1), page(3)].concat());
        // Cached: a second fork maps the same file.
        assert_eq!(assemble(img, &chain).unwrap(), out);
        // A single full image is loaded as it is.
        let solo = dir.path().join("solo");
        std::fs::create_dir(&solo).unwrap();
        std::fs::write(solo.join("mem.0"), "m").unwrap();
        assert_eq!(
            assemble(&solo, &["mem.0".into()]).unwrap(),
            solo.join("mem.0")
        );
    }

    /// Images are whole on their own: linking a chain into a new image and
    /// then deleting the old one leaves the new one loadable.
    #[test]
    fn a_linked_chain_outlives_the_image_it_came_from() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::write(a.join("mem.0"), "zero").unwrap();
        std::fs::write(a.join("mem.1"), "one").unwrap();
        std::fs::create_dir(&b).unwrap();
        std::fs::write(b.join("mem.0"), "stale").unwrap();
        link_chain(&a, &chain_of(&a).unwrap(), &b).unwrap();
        std::fs::remove_dir_all(&a).unwrap();
        assert_eq!(std::fs::read_to_string(b.join("mem.0")).unwrap(), "zero");
        assert_eq!(std::fs::read_to_string(b.join("mem.1")).unwrap(), "one");
    }

    #[test]
    fn prepare_records_what_later_calls_need() {
        let dir = TempDir::new().unwrap();
        if !reflinks(dir.path()) {
            return;
        }
        let fc = Firecracker {
            bin: "firecracker".into(),
            kernel: "/k".into(),
            vcpus: 2,
            work_dir: dir.path().to_path_buf(),
            serial_console: true,
        };
        let tmpl = dir.path().join("rootfs.ext4");
        std::fs::write(&tmpl, "disk").unwrap();
        let s = spec(&["app"], &[]);
        let cfg = RuntimeConfig {
            work_dir: dir.path().to_path_buf(),
            ..Default::default()
        };
        let req = BuildRequest {
            id: "sb1",
            template_rootfs: &tmpl,
            spec: &s,
            port: 8000,
            volume: None,
            restore_from: None,
            netns: Some("/var/run/netns/sst-sb1"),
            sandbox_ip: Some(Ipv4Addr::new(10, 86, 0, 2)),
        };
        let b = fc.prepare(&cfg, &req).unwrap();
        assert_eq!(b, dir.path().join("bundles/sb1"));
        let got = fc.config("sb1").unwrap();
        assert_eq!(got.netns.as_deref(), Some("sst-sb1"));
        assert_eq!(got.sandbox_ip, Some(Ipv4Addr::new(10, 86, 0, 2)));
        assert_eq!((got.mem_mib, got.vcpus), (DEFAULT_MEM_MIB, 2));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("layers/sb1/disk/rootfs.ext4")).unwrap(),
            "disk"
        );
        // Again, as after a restart: the disk it has is kept.
        std::fs::write(dir.path().join("layers/sb1/disk/rootfs.ext4"), "written").unwrap();
        fc.prepare(&cfg, &req).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("layers/sb1/disk/rootfs.ext4")).unwrap(),
            "written"
        );
        let vol = BuildRequest {
            volume: Some("v"),
            ..req
        };
        assert!(fc.prepare(&cfg, &vol).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ficlone_is_the_kernels_number() {
        // _IOW(0x94, 9, int): write direction, a 4-byte argument.
        assert_eq!(FICLONE, (1 << 30) | (4 << 16) | (0x94 << 8) | 9);
    }
}
