//! Moving a node to another backend, when the master asks.
//!
//! Which backend a node runs is fixed for the life of its process: the
//! runtime is built once, and every image on its disk is in that engine's
//! format. So a switch is a restart. The installer writes the choice to a
//! file (`--backend-file`, one word: `runsc` or `firecracker`) instead of
//! into the unit's command line, and that file is the only thing a switch
//! changes. The node rewrites it and exits; systemd (`Restart=on-failure`)
//! starts it again, and it comes back on the new engine, with a fingerprint
//! that names it -- which is what moves it to another pool (ADR 6).
//!
//! A node started without the file has its backend in its command line,
//! which the node does not own, and refuses: it says so instead of editing
//! a systemd unit from inside a process that `ProtectSystem=strict` keeps
//! out of /etc anyway.
//!
//! What it refuses, and why:
//!
//! - anything with a bundle here, a warm seed included. A sandbox on disk is
//!   in the old backend's format and could never be restored on the new one.
//!   The master checks the same before it asks, against its own records; this
//!   is the check against what is actually on the disk.
//! - a backend this host cannot run (`runtime::preflight`): a node that
//!   restarted onto one would only fail to start, again and again.

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::runtime::{self, BackendKind, Runtime};

/// What the node exits with to be started again. EX_TEMPFAIL: not a crash,
/// but systemd's `Restart=on-failure` restarts on any non-zero status.
pub const RESTART_EXIT: i32 = 75;

/// How long the node keeps running after it has answered, so the answer
/// reaches the master before the process goes.
const RESTART_AFTER: Duration = Duration::from_millis(1500);

/// Set once a switch is accepted. From then until the restart the node
/// starts nothing, so a create that arrives in the same batch cannot leave
/// a sandbox behind in the backend it is leaving.
static SWITCHING: AtomicBool = AtomicBool::new(false);

pub fn switching() -> bool {
    SWITCHING.load(Ordering::SeqCst)
}

/// The backend a backend file names, or `None` if there is no file yet.
pub fn read(path: &Path) -> Result<Option<BackendKind>> {
    match std::fs::read_to_string(path) {
        Ok(s) => s
            .parse()
            .map(Some)
            .with_context(|| format!("reading the backend from {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Write it whole or not at all: a half-written file would be a node that
/// cannot start.
pub fn write(path: &Path, kind: BackendKind) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, format!("{kind}\n"))
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

/// Everything short of the restart: decide, check, rewrite. Returns what to
/// tell the master. Split from `switch` so a test can run it without the
/// process exiting under it.
pub fn prepare(rt: &Runtime, file: Option<&Path>, want: BackendKind) -> Result<String> {
    let have = rt.config().backend;
    let Some(file) = file else {
        bail!(
            "this node's backend is fixed by its command line (--backend {have}), \
             not by a backend file, so it cannot be switched from the master. \
             Reinstall it with the installer, which sets --backend-file, or \
             change --backend in its unit by hand"
        );
    };
    if want == have {
        return Ok(format!("already on {have}; nothing to do"));
    }
    let here = rt.bundles();
    if !here.is_empty() {
        bail!(
            "{} sandbox{} still on this node ({}); each is in {have}'s format and \
             could never be restored on {want}. Delete them first",
            here.len(),
            if here.len() == 1 { " is" } else { "es are" },
            here.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
        );
    }
    let mut cfg = rt.config().clone();
    cfg.backend = want;
    let engine =
        runtime::preflight(&cfg, want).with_context(|| format!("this host cannot run {want}"))?;
    write(file, want)?;
    Ok(format!(
        "switching from {have} to {want} ({engine}); restarting to come back on it"
    ))
}

/// Accept a switch: stop starting anything, rewrite the file, and exit
/// once the answer has had time to reach the master.
pub fn switch(rt: &Runtime, file: Option<&Path>, want: BackendKind) -> Result<String> {
    if want == rt.config().backend {
        return prepare(rt, file, want);
    }
    // Before the checks, so nothing starts between them and the exit.
    if SWITCHING.swap(true, Ordering::SeqCst) {
        bail!("a backend switch is already under way; the node is restarting");
    }
    match prepare(rt, file, want) {
        Ok(msg) => {
            tracing::warn!(from = %rt.config().backend, to = %want, "backend switch accepted; exiting to restart");
            std::thread::spawn(|| {
                std::thread::sleep(RESTART_AFTER);
                std::process::exit(RESTART_EXIT);
            });
            Ok(msg)
        }
        Err(e) => {
            SWITCHING.store(false, Ordering::SeqCst);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::RuntimeConfig;

    fn node(dir: &Path) -> Runtime {
        Runtime::new(RuntimeConfig {
            work_dir: dir.join("work"),
            volume_root: dir.join("volumes"),
            // Nothing that could answer, so a preflight of runsc fails here.
            runsc: dir.join("no-runsc").to_string_lossy().into_owned(),
            ..RuntimeConfig::default()
        })
        .unwrap()
    }

    #[test]
    fn the_file_reads_back_what_was_written_and_takes_gvisor_for_runsc() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("backend");
        assert_eq!(read(&f).unwrap(), None, "no file is not an error");
        write(&f, BackendKind::Firecracker).unwrap();
        assert_eq!(read(&f).unwrap(), Some(BackendKind::Firecracker));
        std::fs::write(&f, "gvisor\n").unwrap();
        assert_eq!(read(&f).unwrap(), Some(BackendKind::Runsc));
        std::fs::write(&f, "kata").unwrap();
        assert!(
            read(&f).is_err(),
            "an unknown backend is refused, not defaulted"
        );
    }

    #[test]
    fn a_node_without_a_backend_file_says_why_it_cannot_switch() {
        let dir = tempfile::tempdir().unwrap();
        let e = prepare(&node(dir.path()), None, BackendKind::Firecracker).unwrap_err();
        assert!(format!("{e:#}").contains("--backend-file"), "{e:#}");
    }

    #[test]
    fn the_same_backend_is_a_no_op_and_leaves_the_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("backend");
        let msg = prepare(&node(dir.path()), Some(&f), BackendKind::Runsc).unwrap();
        assert!(msg.contains("already"), "{msg}");
        assert!(!f.exists());
    }

    #[test]
    fn anything_on_disk_blocks_a_switch_seeds_included() {
        let dir = tempfile::tempdir().unwrap();
        let rt = node(dir.path());
        let f = dir.path().join("backend");
        write(&f, BackendKind::Runsc).unwrap();
        let seed = format!("{}abc", crate::node::WARM_SEED_PREFIX);
        std::fs::create_dir_all(dir.path().join("work/bundles").join(&seed)).unwrap();
        let e = prepare(&rt, Some(&f), BackendKind::Firecracker).unwrap_err();
        assert!(format!("{e:#}").contains(&seed), "{e:#}");
        assert_eq!(read(&f).unwrap(), Some(BackendKind::Runsc), "unchanged");
    }

    #[test]
    fn a_backend_this_host_cannot_run_is_refused_before_the_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        // Configured for Firecracker (with a kernel that is not there) so
        // the switch is to runsc, whose binary here does not exist.
        let mut rt = node(dir.path());
        rt = Runtime::with_backend(
            RuntimeConfig {
                backend: BackendKind::Firecracker,
                ..rt.config().clone()
            },
            std::sync::Arc::new(crate::node::Runsc {
                bin: "unused".into(),
                root: dir.path().join("root"),
                work_dir: dir.path().join("work"),
            }),
        )
        .unwrap();
        let f = dir.path().join("backend");
        write(&f, BackendKind::Firecracker).unwrap();
        let e = prepare(&rt, Some(&f), BackendKind::Runsc).unwrap_err();
        assert!(format!("{e:#}").contains("cannot run runsc"), "{e:#}");
        assert_eq!(read(&f).unwrap(), Some(BackendKind::Firecracker));
    }
}
