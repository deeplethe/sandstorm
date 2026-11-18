//! Building an OCI bundle for one sandbox.
//!
//! A template is a rootfs directory on the node plus a small JSON spec. The
//! node turns that into a full OCI spec; the template never carries the
//! security-relevant parts, so a template cannot widen what a sandbox may do.
//!
//! The per-child writable layer is an overlay, not a reflink copy. bm's root
//! filesystem is ext4, which has no reflink at all -- the earlier reflink
//! measurement had to build a loopback XFS to run. Overlay needs nothing from
//! the filesystem and is what container runtimes use for exactly this.
//!
//! The cgroup is named in the spec (`linux.cgroupsPath`), so runsc creates it
//! and the node knows the one path it must write to freeze the sandbox. That
//! is the proven arrangement from the tier probes.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command as Exec;

use super::runtime::RuntimeConfig;

/// The part of a template an operator gets to choose.
#[derive(Debug, Clone, Deserialize)]
pub struct TemplateSpec {
    /// What to run. `{{PORT}}` in any argument is replaced with the
    /// sandbox's port, which is how a template stays port-agnostic.
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// Host directories to bind read-only, so a template needs only a
    /// skeleton rootfs rather than a whole distribution.
    ///
    /// These are the host's own files, so what goes here is a security
    /// decision, not a convenience one. See `FORBIDDEN_BINDS`.
    #[serde(default = "default_ro_binds")]
    pub ro_binds: Vec<String>,
    /// Hard memory ceiling. Without one, a single sandbox can take the node
    /// down, which the density work makes more likely rather than less.
    #[serde(default)]
    pub memory_max_mib: Option<u64>,
    /// How `eval` runs a snippet: these arguments with the code appended.
    /// A template that is not a Python one overrides it.
    #[serde(default = "default_eval_argv")]
    pub eval_argv: Vec<String>,
    /// Start new sandboxes from an image of this template already booted,
    /// instead of booting each one. Opt-in, because every sandbox restored
    /// from one image starts with the same memory: see `WarmSpec`.
    #[serde(default)]
    pub warm: Option<WarmSpec>,
}

/// How a template is kept warm.
///
/// Every sandbox restored from the image begins with the seed's memory,
/// and that includes any random state the program drew before the image
/// was taken. The kernel's randomness is fresh after a restore -- measured
/// on bm, `os.urandom` differs in every child -- but anything seeded in
/// user space is not: Python's `random`, OpenSSL's DRBG and the string hash
/// seed came out identical in a parent and two children. Identical TLS
/// randomness across sandboxes is a security bug, not a curiosity.
///
/// So the template says how to reseed. `after_restore` runs inside each
/// sandbox right after it is restored and before it is given any traffic;
/// a failure there means a cold start instead. For Python that is a signal
/// the program handles by calling `random.seed()` and `ssl.RAND_add()`.
/// The same contract as Lambda SnapStart's and CRaC's after-restore hooks.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WarmSpec {
    #[serde(default)]
    pub after_restore: Vec<String>,
    /// How long the seed may take to start listening before the image is
    /// given up on.
    #[serde(default = "default_ready_timeout")]
    pub ready_timeout_secs: u64,
}

fn default_ready_timeout() -> u64 {
    60
}

fn default_eval_argv() -> Vec<String> {
    ["/usr/bin/python3", "-c"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// The resolvers a sandbox gets when its template names none it can use.
/// Public ones, so a sandbox allowed the internet can use them without being
/// allowed anything else. The same as `sandstorm-init.sh`.
///
/// `single-request-reopen`: a program's A and AAAA lookups leave together from
/// one port, and NAT can lose one of the two, which glibc waits out. Measured
/// from a Firecracker guest: 5.2 s, the same lookup otherwise 0.2 s.
const RESOLVERS: &str =
    "options single-request-reopen timeout:2\nnameserver 1.1.1.1\nnameserver 8.8.8.8\n";

/// Whether this rootfs names a resolver a sandbox can reach. An image often
/// does not: a Docker export leaves `resolv.conf` empty or a link to the
/// systemd-resolved stub, whose 127.0.0.53 is nothing inside a sandbox.
fn has_a_resolver(rootfs: &Path) -> bool {
    let conf = rootfs.join("etc/resolv.conf");
    if !conf.symlink_metadata().is_ok_and(|m| m.is_file()) {
        return false;
    }
    std::fs::read_to_string(conf).is_ok_and(|text| {
        text.lines().any(|l| {
            let mut words = l.split_whitespace();
            words.next() == Some("nameserver")
                && words
                    .next()
                    .and_then(|ip| ip.parse::<std::net::IpAddr>().ok())
                    .is_some_and(|ip| !ip.is_loopback())
        })
    })
}

/// Into the sandbox's own layer, before the overlay is mounted. Left alone
/// if the layer has one, which is the sandbox's business.
fn give_a_resolver(template_rootfs: &Path, upper: &Path) -> Result<()> {
    let conf = upper.join("etc/resolv.conf");
    if conf.symlink_metadata().is_ok() || has_a_resolver(template_rootfs) {
        return Ok(());
    }
    std::fs::create_dir_all(upper.join("etc"))?;
    std::fs::write(conf, RESOLVERS)?;
    Ok(())
}

/// Code directories only. `/etc` is deliberately absent.
///
/// It used to be here, so a template could be a bare skeleton and still
/// resolve users and TLS roots. Measured on bm: a sandbox running as uid 0
/// could then read the host's `/etc/shadow`, `/etc/sudoers` and -- worst --
/// `/etc/ssh/ssh_host_ed25519_key`. A sandbox exists to run code we do not
/// trust, so handing it the host's SSH host keys defeats the entire point.
///
/// A template that needs `/etc/passwd` or TLS roots carries its own, which
/// is what a real rootfs would have done from the start.
fn default_ro_binds() -> Vec<String> {
    ["/usr", "/lib", "/lib64", "/bin", "/sbin"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Host paths a template may never bind, whatever it asks for.
///
/// Refused rather than filtered, so a template asking for one fails loudly
/// instead of running with less than it expected. The node operator can
/// still bind whatever they like by building it into the template's rootfs;
/// what this stops is a template reaching into the host's secrets.
const FORBIDDEN_BINDS: &[&str] = &[
    "/etc",  // shadow, sudoers, ssh host keys
    "/root", // the operator's own home
    "/home", // everyone else's
    "/boot", "/var", // logs, docker state, cloud-init data
    "/run", // sockets, credentials
    "/proc", "/sys", "/dev", "/",
];

/// Is this bind one we refuse? Compared by path component, so `/etc` catches
/// `/etc/ssh` and `/etcetera` is left alone.
fn forbidden_bind(path: &str) -> Option<&'static str> {
    let p = Path::new(path);
    FORBIDDEN_BINDS.iter().copied().find(|f| {
        let fp = Path::new(f);
        if *f == "/" {
            // Only the root itself, or starts_with would match everything.
            p == fp
        } else {
            p.starts_with(fp)
        }
    })
}

/// Standard runc default capability set, minus the ones a sandbox running
/// untrusted code has no business holding.
const CAPS: &[&str] = &[
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_FSETID",
    "CAP_FOWNER",
    "CAP_SETGID",
    "CAP_SETUID",
    "CAP_SETPCAP",
    "CAP_NET_BIND_SERVICE",
    "CAP_KILL",
];

/// What one sandbox's bundle is built from.
pub struct BuildRequest<'a> {
    pub id: &'a str,
    pub template_rootfs: &'a Path,
    pub spec: &'a TemplateSpec,
    /// Port inside the sandbox's own namespace.
    pub port: u16,
    pub volume: Option<&'a str>,
    /// Archive of a previously detached sandbox's upper directory.
    pub restore_from: Option<&'a Path>,
    /// Prepared network namespace to join.
    pub netns: Option<&'a str>,
    /// The sandbox's address in that namespace. runsc's sandbox simply has
    /// it; a VM's guest sits behind it and its backend needs to know.
    pub sandbox_ip: Option<std::net::Ipv4Addr>,
}

pub struct Layout {
    pub bundle: PathBuf,
    pub merged: PathBuf,
    upper: PathBuf,
    work: PathBuf,
}

impl Layout {
    pub fn for_id(cfg: &RuntimeConfig, id: &str) -> Self {
        let layer = cfg.work_dir.join("layers").join(id);
        Self {
            bundle: cfg.work_dir.join("bundles").join(id),
            merged: layer.join("merged"),
            upper: layer.join("upper"),
            work: layer.join("work"),
        }
    }
}

/// Create the writable layer, mount it, and write the bundle's config.json.
/// Idempotent: re-running for a sandbox that already has a mounted layer
/// leaves it alone, which is what makes re-adoption after a restart free.
///
/// `restore_from`, when given, is an archive of a previously detached
/// sandbox's upper directory. It is unpacked BEFORE the overlay is mounted:
/// overlayfs does not guarantee that changes made to an upperdir while it is
/// mounted are visible, so unpacking afterwards would appear to work and
/// then silently lose the sandbox's state.
pub fn build(cfg: &RuntimeConfig, req: &BuildRequest<'_>) -> Result<Layout> {
    let BuildRequest {
        id,
        template_rootfs,
        spec,
        port,
        volume,
        restore_from,
        netns,
        sandbox_ip: _,
    } = *req;
    if !template_rootfs.is_dir() {
        bail!(
            "template rootfs {} is not a directory",
            template_rootfs.display()
        );
    }
    let l = Layout::for_id(cfg, id);
    for d in [&l.bundle, &l.merged, &l.upper, &l.work] {
        std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
    }

    if !is_mounted(&l.merged)? {
        if let Some(arc) = restore_from {
            unpack_into(arc, &l.upper)
                .with_context(|| format!("restoring {id} from {}", arc.display()))?;
        }
        give_a_resolver(template_rootfs, &l.upper).context("writing resolv.conf")?;
        mount_overlay(template_rootfs, &l.upper, &l.work, &l.merged)?;
    } else if restore_from.is_some() {
        // The layer is already live, so the archive cannot be laid down
        // underneath it. Say so rather than appearing to restore.
        bail!("{id} already has a mounted layer; it cannot be restored over");
    }

    let config = oci_spec(id, &l.merged, spec, port, volume, netns)?;
    std::fs::write(
        l.bundle.join("config.json"),
        serde_json::to_vec_pretty(&config)?,
    )
    .context("writing config.json")?;
    Ok(l)
}

/// Unmount the overlay, leaving the layer's directories in place.
///
/// Separate from `teardown` so detach can unmount BEFORE archiving the
/// upper directory. Reading a live upperdir is the same class of thing
/// overlayfs warns about for writing it, and there is no reason to: by
/// detach time the sandbox is already checkpointed and gone.
pub fn unmount(cfg: &RuntimeConfig, id: &str) -> Result<()> {
    let l = Layout::for_id(cfg, id);
    if !is_mounted(&l.merged).unwrap_or(false) {
        return Ok(());
    }
    match Exec::new("umount").arg(&l.merged).output() {
        Ok(o) if !o.status.success() => {
            bail!(
                "unmounting {} failed: {}",
                l.merged.display(),
                String::from_utf8_lossy(&o.stderr).trim()
            );
        }
        Err(e) => bail!("umount failed to run: {e}"),
        _ => Ok(()),
    }
}

/// Unmount and delete the layer. Called on destroy, and on detach once the
/// layer has been archived to the volume.
pub fn teardown(cfg: &RuntimeConfig, id: &str) -> Result<()> {
    let l = Layout::for_id(cfg, id);
    if is_mounted(&l.merged).unwrap_or(false) {
        let out = Exec::new("umount").arg(&l.merged).output();
        match out {
            Ok(o) if !o.status.success() => {
                // Lazy unmount rather than leaving a mount behind: the
                // directory is about to be removed either way.
                let _ = Exec::new("umount").arg("-l").arg(&l.merged).status();
            }
            Err(e) => tracing::warn!(error = %e, "umount failed to run"),
            _ => {}
        }
    }
    std::fs::remove_dir_all(cfg.work_dir.join("layers").join(id)).ok();
    std::fs::remove_dir_all(&l.bundle).ok();
    Ok(())
}

pub(super) fn unpack_into(archive: &Path, dest: &Path) -> Result<()> {
    let status = Exec::new("sh")
        .arg("-c")
        .arg(format!(
            "zstd -dq -T0 -c {} | tar --xattrs --xattrs-include='trusted.overlay.*' --xattrs-include='user.overlay.*' -C {} -xf -",
            shell_quote(&archive.to_string_lossy()),
            shell_quote(&dest.to_string_lossy())
        ))
        .status()
        .context("unpacking the archived layer")?;
    if !status.success() {
        bail!("unpacking {} failed with {status}", archive.display());
    }
    Ok(())
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn is_mounted(p: &Path) -> Result<bool> {
    let out = Exec::new("mountpoint").arg("-q").arg(p).status();
    Ok(matches!(out, Ok(s) if s.success()))
}

fn mount_overlay(lower: &Path, upper: &Path, work: &Path, merged: &Path) -> Result<()> {
    // Commas and colons separate overlay options, so a path containing one
    // would silently produce a different mount.
    for p in [lower, upper, work, merged] {
        let s = p.to_string_lossy();
        if s.contains(',') || s.contains(':') {
            bail!("path {s} contains a character overlay options cannot carry");
        }
    }
    let opts = format!(
        "lowerdir={},upperdir={},workdir={}",
        lower.display(),
        upper.display(),
        work.display()
    );
    let out = Exec::new("mount")
        .args(["-t", "overlay", "overlay", "-o"])
        .arg(&opts)
        .arg(merged)
        .output()
        .context("running mount -t overlay")?;
    if !out.status.success() {
        bail!(
            "overlay mount failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn oci_spec(
    id: &str,
    root: &Path,
    spec: &TemplateSpec,
    port: u16,
    volume: Option<&str>,
    netns: Option<&str>,
) -> Result<serde_json::Value> {
    let args: Vec<String> = spec
        .args
        .iter()
        .map(|a| a.replace("{{PORT}}", &port.to_string()))
        .collect();
    if args.is_empty() {
        bail!("template has no args, so there is nothing to run");
    }

    let mut env = vec![
        "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
        "HOME=/root".to_string(),
        // Stops the sandbox writing .pyc files into the read-only binds on
        // every start, which shows up as noise and wasted work.
        "PYTHONDONTWRITEBYTECODE=1".to_string(),
    ];
    env.extend(spec.env.iter().cloned());

    let mut mounts = vec![
        json!({"destination": "/proc", "type": "proc", "source": "proc"}),
        json!({"destination": "/dev", "type": "tmpfs", "source": "tmpfs",
               "options": ["nosuid", "noexec", "strictatime", "mode=755", "size=65536k"]}),
        json!({"destination": "/dev/pts", "type": "devpts", "source": "devpts",
               "options": ["nosuid", "noexec", "newinstance", "ptmxmode=0666", "mode=0620"]}),
        json!({"destination": "/dev/shm", "type": "tmpfs", "source": "shm",
               "options": ["nosuid", "noexec", "nodev", "mode=1777", "size=65536k"]}),
        json!({"destination": "/sys", "type": "sysfs", "source": "sysfs",
               "options": ["nosuid", "noexec", "nodev", "ro"]}),
        json!({"destination": "/tmp", "type": "tmpfs", "source": "tmpfs",
               "options": ["nosuid", "nodev", "mode=1777", "size=262144k"]}),
    ];
    for b in &spec.ro_binds {
        if let Some(why) = forbidden_bind(b) {
            bail!(
                "template asks to bind {b}, which is under {why}: that would give \
                 a sandbox the host's own files. Build what it needs into the \
                 template's rootfs instead."
            );
        }
        if !Path::new(b).exists() {
            continue;
        }
        mounts.push(json!({
            "destination": b, "source": b, "type": "bind",
            "options": ["rbind", "ro", "nosuid", "nodev"]
        }));
    }
    // The volume is the only thing that survives the detached tier, so it is
    // the one writable mount that is not part of the throwaway layer.
    if let Some(v) = volume {
        std::fs::create_dir_all(v).with_context(|| format!("creating volume {v}"))?;
        mounts.push(json!({
            "destination": "/workspace", "source": v, "type": "bind",
            "options": ["rbind", "rw", "nosuid", "nodev"]
        }));
    }

    // A network namespace with a PATH, so the runtime joins the one the node
    // prepared. Without it the sandbox shares the host's namespace, which
    // let it reach the master's API, the host's sshd and its siblings.
    let mut namespaces = vec![
        json!({"type": "pid"}),
        json!({"type": "ipc"}),
        json!({"type": "uts"}),
        json!({"type": "mount"}),
    ];
    match netns {
        Some(path) => namespaces.push(json!({"type": "network", "path": path})),
        // No namespace entry at all would share the host's. An unnamed one
        // gives the sandbox a fresh, empty namespace instead: no addresses,
        // no route out. Less useful than a configured one, never less safe.
        None => namespaces.push(json!({"type": "network"})),
    }

    let mut resources = json!({});
    if let Some(mib) = spec.memory_max_mib {
        resources["memory"] = json!({ "limit": mib * 1024 * 1024 });
    }

    Ok(json!({
        "ociVersion": "1.0.2",
        "process": {
            "terminal": false,
            "user": { "uid": 0, "gid": 0 },
            "args": args,
            "env": env,
            "cwd": spec.cwd.clone().unwrap_or_else(|| "/".to_string()),
            "capabilities": {
                "bounding": CAPS, "effective": CAPS, "permitted": CAPS
            },
            "rlimits": [{ "type": "RLIMIT_NOFILE", "hard": 8192, "soft": 8192 }],
            "noNewPrivileges": true
        },
        "root": { "path": root.to_string_lossy(), "readonly": false },
        "hostname": id,
        "mounts": mounts,
        "linux": {
            "namespaces": namespaces,
            "cgroupsPath": format!("/sandstorm/{id}"),
            "resources": resources
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> TemplateSpec {
        TemplateSpec {
            args: vec![
                "/usr/bin/python3".into(),
                "/app/p.py".into(),
                "{{PORT}}".into(),
            ],
            env: vec!["FOO=bar".into()],
            cwd: None,
            ro_binds: default_ro_binds(),
            memory_max_mib: Some(256),
            eval_argv: default_eval_argv(),
            warm: None,
        }
    }

    #[test]
    fn the_port_reaches_the_command_line() {
        let s = oci_spec("sb1", Path::new("/r"), &spec(), 19900, None, None).unwrap();
        let args: Vec<String> = serde_json::from_value(s["process"]["args"].clone()).unwrap();
        assert_eq!(args[2], "19900", "{{PORT}} must be substituted");
    }

    #[test]
    fn the_cgroup_is_named_so_freeze_knows_where_to_write() {
        let s = oci_spec("sb1", Path::new("/r"), &spec(), 1, None, None).unwrap();
        assert_eq!(s["linux"]["cgroupsPath"], "/sandstorm/sb1");
    }

    #[test]
    fn a_memory_ceiling_reaches_the_spec() {
        let s = oci_spec("sb1", Path::new("/r"), &spec(), 1, None, None).unwrap();
        assert_eq!(
            s["linux"]["resources"]["memory"]["limit"],
            256 * 1024 * 1024
        );
    }

    /// A template must not be able to widen what a sandbox may do.
    #[test]
    fn the_security_relevant_parts_are_not_the_templates_to_choose() {
        let s = oci_spec("sb1", Path::new("/r"), &spec(), 1, None, None).unwrap();
        assert_eq!(s["process"]["noNewPrivileges"], true);
        let caps: Vec<String> =
            serde_json::from_value(s["process"]["capabilities"]["effective"].clone()).unwrap();
        for forbidden in [
            "CAP_SYS_ADMIN",
            "CAP_SYS_MODULE",
            "CAP_SYS_PTRACE",
            "CAP_NET_RAW",
        ] {
            assert!(
                !caps.contains(&forbidden.to_string()),
                "{forbidden} granted"
            );
        }
    }

    #[test]
    fn env_from_the_template_is_added_not_substituted() {
        let s = oci_spec("sb1", Path::new("/r"), &spec(), 1, None, None).unwrap();
        let env: Vec<String> = serde_json::from_value(s["process"]["env"].clone()).unwrap();
        assert!(env.iter().any(|e| e.starts_with("PATH=")), "PATH kept");
        assert!(env.contains(&"FOO=bar".to_string()), "template env added");
    }

    #[test]
    fn a_volume_is_the_one_writable_mount_outside_the_layer() {
        let td = tempfile::TempDir::new().unwrap();
        let vol = td.path().join("vol");
        let s = oci_spec(
            "sb1",
            Path::new("/r"),
            &spec(),
            1,
            Some(vol.to_str().unwrap()),
            None,
        )
        .unwrap();
        let mounts: Vec<serde_json::Value> = serde_json::from_value(s["mounts"].clone()).unwrap();
        let ws = mounts
            .iter()
            .find(|m| m["destination"] == "/workspace")
            .expect("volume mounted at /workspace");
        let opts: Vec<String> = serde_json::from_value(ws["options"].clone()).unwrap();
        assert!(opts.contains(&"rw".to_string()));
        // Every other bind stays read-only.
        for m in mounts.iter().filter(|m| m["type"] == "bind") {
            let o: Vec<String> = serde_json::from_value(m["options"].clone()).unwrap();
            if m["destination"] != "/workspace" {
                assert!(
                    o.contains(&"ro".to_string()),
                    "{:?} is not ro",
                    m["destination"]
                );
            }
        }
    }

    /// Measured on bm before this was fixed: a uid-0 sandbox read the
    /// host's /etc/shadow, /etc/sudoers and /etc/ssh/ssh_host_ed25519_key.
    #[test]
    fn the_host_etc_is_not_bound_by_default() {
        let s = oci_spec("sb1", Path::new("/r"), &spec(), 1, None, None).unwrap();
        let mounts: Vec<serde_json::Value> = serde_json::from_value(s["mounts"].clone()).unwrap();
        for m in &mounts {
            let d = m["destination"].as_str().unwrap_or("");
            assert_ne!(d, "/etc", "the host's /etc must not be bound");
        }
        // The code directories are still requested, so a thin template
        // works. Asserted on the list rather than the generated mounts,
        // since a path absent from the build host is skipped.
        let defaults = default_ro_binds();
        assert!(
            !defaults.iter().any(|b| b == "/etc"),
            "/etc must not be a default"
        );
        for want in ["/usr", "/lib", "/bin", "/sbin"] {
            assert!(
                defaults.iter().any(|b| b == want),
                "{want} should be a default"
            );
        }
    }

    #[test]
    fn a_template_cannot_ask_for_the_hosts_secrets() {
        for bad in [
            "/etc",
            "/etc/ssh",
            "/root",
            "/root/.ssh",
            "/home/ubuntu",
            "/var/lib/docker",
            "/run/secrets",
            "/proc",
            "/sys",
            "/",
        ] {
            let mut sp = spec();
            sp.ro_binds = vec![bad.to_string()];
            let e = oci_spec("sb1", Path::new("/r"), &sp, 1, None, None)
                .unwrap_err()
                .to_string();
            assert!(e.contains("host's own files"), "{bad} was allowed: {e}");
        }
    }

    /// Matched by path component, so a legitimate path that merely starts
    /// with the same letters is not caught.
    #[test]
    fn the_deny_list_matches_components_not_prefixes() {
        assert!(forbidden_bind("/etc").is_some());
        assert!(forbidden_bind("/etc/ssh").is_some());
        assert!(forbidden_bind("/etcetera").is_none());
        assert!(forbidden_bind("/usr").is_none());
        assert!(forbidden_bind("/opt/myapp").is_none());
    }

    #[test]
    fn a_template_with_nothing_to_run_is_refused() {
        let mut s = spec();
        s.args.clear();
        assert!(oci_spec("sb1", Path::new("/r"), &s, 1, None, None).is_err());
    }

    /// Overlay options are comma and colon separated, so a path carrying
    /// one would quietly produce a different mount.
    #[test]
    fn overlay_refuses_a_path_it_cannot_express() {
        let bad = Path::new("/var/lib/sandstorm/a,b");
        let e = mount_overlay(bad, bad, bad, bad).unwrap_err().to_string();
        assert!(e.contains("cannot carry"), "got: {e}");
    }
}

#[cfg(test)]
mod resolver_tests {
    use super::*;

    fn conf(lower: &[(&str, &str)]) -> (tempfile::TempDir, std::path::PathBuf) {
        let td = tempfile::TempDir::new().unwrap();
        for (path, text) in lower {
            let p = td.path().join("lower").join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        std::fs::create_dir_all(td.path().join("lower")).unwrap();
        let upper = td.path().join("upper");
        std::fs::create_dir_all(&upper).unwrap();
        (td, upper)
    }

    fn resolved(lower: &[(&str, &str)]) -> Option<String> {
        let (td, upper) = conf(lower);
        give_a_resolver(&td.path().join("lower"), &upper).unwrap();
        std::fs::read_to_string(upper.join("etc/resolv.conf")).ok()
    }

    #[test]
    fn an_image_with_no_usable_resolver_gets_one() {
        assert_eq!(resolved(&[]).as_deref(), Some(RESOLVERS));
        let empty = [("etc/resolv.conf", "")];
        assert_eq!(resolved(&empty).as_deref(), Some(RESOLVERS));
        let stub = [("etc/resolv.conf", "nameserver 127.0.0.53\noptions edns0\n")];
        assert_eq!(resolved(&stub).as_deref(), Some(RESOLVERS));
    }

    #[test]
    fn a_template_that_names_one_keeps_it() {
        assert_eq!(
            resolved(&[("etc/resolv.conf", "nameserver 9.9.9.9\n")]),
            None
        );
    }

    #[test]
    fn a_link_out_of_the_rootfs_is_not_followed() {
        let (td, upper) = conf(&[]);
        let etc = td.path().join("lower/etc");
        std::fs::create_dir_all(&etc).unwrap();
        std::os::unix::fs::symlink(
            "/run/systemd/resolve/stub-resolv.conf",
            etc.join("resolv.conf"),
        )
        .unwrap();
        give_a_resolver(&td.path().join("lower"), &upper).unwrap();
        assert_eq!(
            std::fs::read_to_string(upper.join("etc/resolv.conf")).unwrap(),
            RESOLVERS
        );
    }

    #[test]
    fn what_the_sandbox_wrote_stays() {
        let (td, upper) = conf(&[]);
        std::fs::create_dir_all(upper.join("etc")).unwrap();
        std::fs::write(upper.join("etc/resolv.conf"), "mine").unwrap();
        give_a_resolver(&td.path().join("lower"), &upper).unwrap();
        assert_eq!(
            std::fs::read_to_string(upper.join("etc/resolv.conf")).unwrap(),
            "mine"
        );
    }
}

#[cfg(test)]
mod netns_tests {
    use super::*;

    #[test]
    fn a_prepared_namespace_is_joined_by_path() {
        let s = oci_spec(
            "sb1",
            Path::new("/r"),
            &spec_for_test(),
            1,
            None,
            Some("/var/run/netns/sandstorm-7"),
        )
        .unwrap();
        let ns: Vec<serde_json::Value> =
            serde_json::from_value(s["linux"]["namespaces"].clone()).unwrap();
        let net = ns
            .iter()
            .find(|n| n["type"] == "network")
            .expect("a network namespace is declared");
        assert_eq!(net["path"], "/var/run/netns/sandstorm-7");
    }

    /// Without a prepared namespace the sandbox still gets its own, empty
    /// one -- never the host's.
    #[test]
    fn without_one_it_still_does_not_share_the_hosts() {
        let s = oci_spec("sb1", Path::new("/r"), &spec_for_test(), 1, None, None).unwrap();
        let ns: Vec<serde_json::Value> =
            serde_json::from_value(s["linux"]["namespaces"].clone()).unwrap();
        let net = ns
            .iter()
            .find(|n| n["type"] == "network")
            .expect("a network namespace is still declared");
        assert!(
            net.get("path").is_none(),
            "an unnamed namespace is a fresh one"
        );
    }

    fn spec_for_test() -> TemplateSpec {
        TemplateSpec {
            args: vec!["/bin/true".into()],
            env: vec![],
            cwd: None,
            ro_binds: default_ro_binds(),
            memory_max_mib: None,
            eval_argv: default_eval_argv(),
            warm: None,
        }
    }
}
