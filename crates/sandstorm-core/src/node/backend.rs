//! The engine underneath a node: the few operations that differ between
//! gVisor and anything else that could run a sandbox.
//!
//! A backend prepares a sandbox's writable layer and its bundle directory
//! from a template -- an overlay and an OCI spec for runsc, a reflinked disk
//! image and a VM description for Firecracker -- and is then asked to start,
//! checkpoint, restore or remove the sandbox from them. Everything else
//! `Runtime` does is the node's own and stays there: the network namespace,
//! tags, archives, and every tier change that is a cgroup write.
//!
//! `bundles/<id>/config.json` existing is what "this sandbox is on this
//! node" means to `Runtime`, whatever the backend writes in it.
//!
//! What `Runtime` relies on, and a backend must therefore keep true:
//!
//!   - Every process of a sandbox lives in the cgroup the bundle names
//!     (`linux.cgroupsPath`, `/sys/fs/cgroup/sandstorm/<id>`). Frozen is a
//!     write to that cgroup's `cgroup.freeze`, `observe` reads its
//!     `cgroup.procs`, density reads `memory.current`, and `destroy` falls
//!     back to `cgroup.kill`. A backend that started anything outside it
//!     would leave that thing running through a freeze and a destroy.
//!   - A started sandbox does not belong to this process. A node restart
//!     must find it still running and adopt it from the bundle directory.
//!   - Nothing here blocks unbounded. Each call has its own deadline, so a
//!     wedged engine fails one sandbox instead of stopping the node's loop.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use super::bundle::{self, BuildRequest};
use super::runtime::RuntimeConfig;
use std::process::{Command as Exec, Stdio};
use std::time::{Duration, Instant};

/// See the module documentation for what an implementation must keep true.
pub trait Backend: Send + Sync + 'static {
    /// Identifies this engine's build. Part of the node's fingerprint, so
    /// it must change whenever an image this engine wrote might not restore.
    fn version(&self) -> String;

    /// What a template installed at `dir` (`templates/<name>`) is made of
    /// for this backend. A template is not installed if this does not exist.
    fn template_root(&self, dir: &Path) -> PathBuf;

    /// Lay down the sandbox's writable layer and write its bundle directory,
    /// returning that directory. Idempotent: a sandbox that already has a
    /// layer keeps it, which is what makes re-adoption after a restart free.
    fn prepare(&self, cfg: &RuntimeConfig, req: &BuildRequest<'_>) -> Result<PathBuf>;

    /// The directory holding everything this sandbox wrote, which detach
    /// archives and a later `prepare` with `restore_from` unpacks again.
    fn layer(&self, cfg: &RuntimeConfig, id: &str) -> PathBuf;

    /// Stop using the layer, leaving it in place to be archived.
    fn unmount(&self, cfg: &RuntimeConfig, id: &str) -> Result<()>;

    /// Remove the layer and the bundle directory.
    fn teardown(&self, cfg: &RuntimeConfig, id: &str) -> Result<()>;

    /// Start a sandbox from its bundle, from nothing.
    fn boot(&self, id: &str, bundle: &Path) -> Result<()>;

    /// Start a sandbox from its bundle and a checkpoint image. The image is
    /// only read: one image serves any number of restores.
    fn restore(&self, id: &str, bundle: &Path, image: &Path) -> Result<()>;

    /// Write a running sandbox's state into `image`, an existing directory.
    /// The sandbox is stopped afterwards; `remove` is still needed.
    fn checkpoint(&self, id: &str, image: &Path, compress: bool) -> Result<()>;
    /// Whether `image` looks like a whole checkpoint of this engine's, from
    /// its files alone. `observe` relies on it, so no subprocess and nothing
    /// slow; it is a layout check, and restore stays the final word.
    fn validate(&self, image: &Path) -> Result<()>;

    /// Forget a stopped sandbox. Best effort: called on paths that are
    /// already cleaning up, where there is nothing better to do on failure.
    fn remove(&self, id: &str);

    /// Kill everything in a sandbox. Best effort, like `remove`; `destroy`
    /// backs it with `cgroup.kill`.
    fn kill(&self, id: &str);

    /// Run a command inside a running sandbox and wait for it, for at most
    /// `timeout`. Returns stdout, stderr and the exit code; a timeout is an
    /// exit code of 124, not an error, since it is the command's result.
    fn exec(&self, id: &str, args: &[String], timeout: Duration) -> Result<(String, String, i64)>;

    /// Whatever this engine needs inside a sandbox's network namespace that
    /// does not depend on the sandbox, done once when the namespace is built
    /// ahead of time (`net::Pool`). Nothing by default: runsc takes the veth
    /// as it is.
    fn prepare_netns(&self, _netns: &str) -> Result<()> {
        Ok(())
    }

    /// Let a sandbox that `checkpoint` just wrote into `image` carry on in
    /// place, `image` being what its memory now stands on. False where the
    /// checkpoint ended it: runsc's stops the container, and the runtime
    /// puts it back by restoring from `image`.
    fn resume_after_checkpoint(&self, _id: &str, _image: &Path) -> Result<bool> {
        Ok(false)
    }

    /// Whether the sandbox's veth must be named and addressed before it
    /// starts. runsc takes the device over as it finds it at boot; an engine
    /// that does not can start while the veth is still being set up
    /// (`net::Pending`).
    fn veth_needed_to_start(&self) -> bool {
        true
    }
}

/// Ceiling for a runsc command that should answer immediately. Generous
/// against a loaded node, short enough that a wedged runsc surfaces as an
/// error on this sandbox rather than a node that has stopped responding.
const RUNSC_TIMEOUT: Duration = Duration::from_secs(20);

/// Checkpoint and restore do real work proportional to the sandbox's memory.
/// Measured at 84 ms on bm, but a large sandbox on a loaded node is slower.
const RUNSC_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(120);

/// gVisor. The default, and the only backend measured so far.
///
/// `runsc create` + `runsc start` daemonises the sandbox, so it is not a
/// child of this process and a control-plane restart does not touch it.
pub struct Runsc {
    pub bin: String,
    /// runsc's state directory.
    pub root: PathBuf,
    /// Sandbox logs and the files short commands write their output to.
    pub work_dir: PathBuf,
}

impl Runsc {
    /// For runsc commands that leave a long-lived process behind: create,
    /// start, restore.
    ///
    /// These must NOT go through `run()`. `Command::output()` reads the
    /// child's stdout and stderr until EOF, and the sandbox process inherits
    /// those pipes and keeps them open for as long as it runs -- so EOF
    /// never arrives and the call blocks for ever on a sandbox that started
    /// perfectly well. The symptom is a container stuck in `created` with no
    /// error anywhere. Give the sandbox a file for its output instead.
    fn spawn(&self, id: &str, args: &[&str]) -> Result<()> {
        let log_path = self.work_dir.join("logs");
        std::fs::create_dir_all(&log_path)?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path.join(format!("{id}.log")))
            .with_context(|| format!("opening the log for {id}"))?;

        let mut child = Exec::new(&self.bin)
            .arg("--root")
            .arg(&self.root)
            .args(args)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .with_context(|| format!("running {} {:?}", self.bin, args))?;

        // Bounded for the same reason as `run`: a wedged runsc must not
        // take the node's loop with it.
        let deadline = Instant::now() + RUNSC_SNAPSHOT_TIMEOUT;
        let status = loop {
            match child.try_wait()? {
                Some(s) => break s,
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!(
                        "{} {:?} did not finish within {:?}",
                        self.bin,
                        args,
                        RUNSC_SNAPSHOT_TIMEOUT
                    );
                }
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        };
        if !status.success() {
            bail!(
                "{} {:?} exited with {status}; see {}",
                self.bin,
                args,
                log_path.join(format!("{id}.log")).display()
            );
        }
        Ok(())
    }

    /// For short-lived runsc commands that answer and exit: delete, kill,
    /// checkpoint.
    ///
    /// Every one of these gets a deadline. runsc takes a lock on its state
    /// directory, and a wedged container makes even `runsc list` block --
    /// which would block the node's whole loop. A node that stops polling
    /// because one container is stuck is worse than one that reports an
    /// error about it.
    ///
    /// Output goes to temporary files rather than pipes. `Command::output()`
    /// waits for EOF, and anything runsc leaves holding those descriptors
    /// keeps them open -- the same trap `spawn` documents.
    fn run(&self, args: &[&str], timeout: Duration) -> Result<String> {
        let tmp = self.work_dir.join("tmp");
        std::fs::create_dir_all(&tmp)?;
        let stem = format!("runsc-{}", std::process::id());
        let (out_path, err_path) = (
            tmp.join(format!("{stem}.out")),
            tmp.join(format!("{stem}.err")),
        );
        let out_file = std::fs::File::create(&out_path)?;
        let err_file = std::fs::File::create(&err_path)?;

        let mut child = Exec::new(&self.bin)
            .arg("--root")
            .arg(&self.root)
            .args(args)
            .stdin(Stdio::null())
            .stdout(out_file)
            .stderr(err_file)
            .spawn()
            .with_context(|| format!("running {} {:?}", self.bin, args))?;

        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait()? {
                Some(s) => break s,
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!(
                        "{} {:?} did not finish within {:?}; runsc may be wedged on \
                         its state directory",
                        self.bin,
                        args,
                        timeout
                    );
                }
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        };

        let stdout = std::fs::read_to_string(&out_path).unwrap_or_default();
        let stderr = std::fs::read_to_string(&err_path).unwrap_or_default();
        std::fs::remove_file(&out_path).ok();
        std::fs::remove_file(&err_path).ok();

        if !status.success() {
            bail!("{} {:?} failed: {}", self.bin, args, stderr.trim());
        }
        Ok(stdout)
    }
}

impl Backend for Runsc {
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
        dir.join("rootfs")
    }

    fn prepare(&self, cfg: &RuntimeConfig, req: &BuildRequest<'_>) -> Result<PathBuf> {
        bundle::build(cfg, req).map(|l| l.bundle)
    }

    fn layer(&self, cfg: &RuntimeConfig, id: &str) -> PathBuf {
        cfg.work_dir.join("layers").join(id).join("upper")
    }

    fn unmount(&self, cfg: &RuntimeConfig, id: &str) -> Result<()> {
        bundle::unmount(cfg, id)
    }

    fn teardown(&self, cfg: &RuntimeConfig, id: &str) -> Result<()> {
        bundle::teardown(cfg, id)
    }

    // create then start, rather than `run`: create reports a bad bundle as
    // an error we can attribute.
    //
    // A failed start must not leave the container behind. Its sandbox process
    // stays in the cgroup with nothing running, `observe` reports that as a
    // running sandbox, and the master clears the failure and never retries.
    fn boot(&self, id: &str, bundle: &Path) -> Result<()> {
        self.spawn(id, &["create", "--bundle", &bundle.to_string_lossy(), id])
            .with_context(|| format!("runsc create {id}"))
            .and_then(|()| {
                self.spawn(id, &["start", id])
                    .with_context(|| format!("runsc start {id}"))
            })
            .inspect_err(|_| self.remove(id))
    }

    fn restore(&self, id: &str, bundle: &Path, image: &Path) -> Result<()> {
        self.spawn(
            id,
            &[
                "restore",
                "--bundle",
                &bundle.to_string_lossy(),
                &format!("--image-path={}", image.display()),
                "--detach",
                id,
            ],
        )
        .inspect_err(|_| self.remove(id))
    }

    fn checkpoint(&self, id: &str, image: &Path, compress: bool) -> Result<()> {
        let image_path = format!("--image-path={}", image.display());
        // Restore reads the compression from the image, so images written
        // either way restore the same and a node can change its mind.
        let mut args = vec!["checkpoint", image_path.as_str()];
        if compress {
            args.push("--compression=flate-best-speed");
        }
        args.push(id);
        self.run(&args, RUNSC_SNAPSHOT_TIMEOUT).map(drop)
    }

    /// `checkpoint.img` opens with the statefile header: "gVisorSF", then
    /// the big-endian length of the JSON metadata that follows it
    /// (gvisor `pkg/state/statefile`). A split image keeps its memory in
    /// `pages_meta.img` and `pages.img` beside it, and needs both.
    fn validate(&self, image: &Path) -> Result<()> {
        use std::io::Read;
        let mut f = std::fs::File::open(image.join("checkpoint.img"))
            .with_context(|| format!("no checkpoint.img in {}", image.display()))?;
        let mut header = [0u8; 16];
        f.read_exact(&mut header)
            .context("checkpoint.img is shorter than its header")?;
        if &header[..8] != b"gVisorSF" {
            bail!("checkpoint.img is not a gVisor statefile");
        }
        let meta = u64::from_be_bytes(header[8..].try_into().unwrap());
        if meta > 16 << 20 || f.metadata()?.len() <= 16 + meta {
            bail!("checkpoint.img is truncated");
        }
        let len = |name: &str| std::fs::metadata(image.join(name)).map(|m| m.len()).ok();
        match (len("pages_meta.img"), len("pages.img")) {
            (Some(meta), Some(_)) if meta > 0 => Ok(()),
            (None, None) => Ok(()),
            _ => bail!("checkpoint pages files are incomplete"),
        }
    }

    fn remove(&self, id: &str) {
        let _ = self.run(&["delete", "-force", id], RUNSC_TIMEOUT);
    }

    fn kill(&self, id: &str) {
        let _ = self.run(&["kill", id, "KILL"], RUNSC_TIMEOUT);
    }

    /// `.output()` would be safe here, unlike for create: an exec'd command
    /// is expected to finish and its output is the whole point. Files all
    /// the same, and the timeout is what protects us if it spawns something
    /// that holds them open.
    fn exec(&self, id: &str, args: &[String], timeout: Duration) -> Result<(String, String, i64)> {
        if args.is_empty() {
            bail!("exec needs a command to run");
        }
        let tmp = self.work_dir.join("tmp");
        std::fs::create_dir_all(&tmp)?;
        let stem = format!("exec-{}-{}", id, std::process::id());
        let (op, ep) = (
            tmp.join(format!("{stem}.out")),
            tmp.join(format!("{stem}.err")),
        );
        let (of, ef) = (std::fs::File::create(&op)?, std::fs::File::create(&ep)?);

        let mut child = Exec::new(&self.bin)
            .arg("--root")
            .arg(&self.root)
            .arg("exec")
            .arg(id)
            .args(args)
            .stdin(Stdio::null())
            .stdout(of)
            .stderr(ef)
            .spawn()
            .with_context(|| format!("runsc exec {id}"))?;

        let deadline = Instant::now() + timeout;
        let mut timed_out = false;
        let status = loop {
            match child.try_wait()? {
                Some(st) => break st,
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break std::process::ExitStatus::default();
                }
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        };

        let stdout = std::fs::read_to_string(&op).unwrap_or_default();
        let mut stderr = std::fs::read_to_string(&ep).unwrap_or_default();
        std::fs::remove_file(&op).ok();
        std::fs::remove_file(&ep).ok();

        if timed_out {
            stderr.push_str(&format!(
                "\nsandstorm: killed after {}s\n",
                timeout.as_secs()
            ));
            return Ok((stdout, stderr, 124));
        }
        Ok((stdout, stderr, status.code().unwrap_or(-1) as i64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A `runsc` that logs each subcommand and fails on `fail_on`.
    ///
    /// Written by a child, not from here. A test forking on another thread
    /// while this process holds the file open for writing keeps that
    /// descriptor until it execs, and running the script then fails with
    /// ETXTBSY, which looks like `runsc` itself failing.
    fn runsc(dir: &TempDir, fail_on: &str) -> Runsc {
        let bin = dir.path().join("runsc");
        let script = format!(
            "#!/bin/sh\nshift 2\necho \"$1\" >> {}/calls\n[ \"$1\" != \"{fail_on}\" ]\n",
            dir.path().display()
        );
        let written = Exec::new("sh")
            .args(["-c", "printf %s \"$1\" > \"$2\" && chmod 755 \"$2\"", "sh"])
            .arg(script)
            .arg(&bin)
            .status()
            .unwrap();
        assert!(written.success());
        Runsc {
            bin: bin.to_string_lossy().into_owned(),
            root: dir.path().join("root"),
            work_dir: dir.path().to_path_buf(),
        }
    }

    fn calls(dir: &TempDir) -> String {
        std::fs::read_to_string(dir.path().join("calls"))
            .unwrap()
            .replace('\n', " ")
    }

    #[test]
    fn a_start_that_fails_removes_the_container() {
        let dir = TempDir::new().unwrap();
        let err = runsc(&dir, "start").boot("sb1", dir.path()).unwrap_err();
        assert!(format!("{err:#}").contains("runsc start sb1"), "{err:#}");
        assert_eq!(calls(&dir), "create start delete ");
    }

    #[test]
    fn a_create_that_fails_is_not_started() {
        let dir = TempDir::new().unwrap();
        let err = runsc(&dir, "create").boot("sb1", dir.path()).unwrap_err();
        assert!(format!("{err:#}").contains("runsc create sb1"), "{err:#}");
        assert_eq!(calls(&dir), "create delete ");
    }

    #[test]
    fn a_restore_that_fails_removes_the_container() {
        let dir = TempDir::new().unwrap();
        let rt = runsc(&dir, "restore");
        let err = rt.restore("sb1", dir.path(), dir.path()).unwrap_err();
        assert_eq!(calls(&dir), "restore delete ", "{err:#}");
    }

    #[test]
    fn a_sandbox_that_started_is_left_alone() {
        let dir = TempDir::new().unwrap();
        runsc(&dir, "nothing").boot("sb1", dir.path()).unwrap();
        assert_eq!(calls(&dir), "create start ");
    }
}
