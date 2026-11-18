use super::*;
use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
    runtime: Runtime,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Runtime::new(RuntimeConfig {
            work_dir: dir.path().join("work"),
            volume_root: dir.path().join("volumes"),
            ..Default::default()
        })
        .unwrap();
        let fixture = Self { dir, runtime };
        fixture.seed();
        fixture
    }

    fn seed(&self) {
        let work = &self.runtime.cfg.work_dir;
        for directory in ["layers/sb1/upper/nested", "snapshots/sb1", "bundles/sb1"] {
            std::fs::create_dir_all(work.join(directory)).unwrap();
        }
        std::fs::write(
            work.join("layers/sb1/upper/nested/it's data"),
            b"\0complete\xff",
        )
        .unwrap();
        std::fs::write(work.join("layers/sb1/upper/other"), b"second file").unwrap();
        std::fs::write(work.join("snapshots/sb1/checkpoint"), b"snapshot").unwrap();
    }

    fn assert_source_preserved(&self) {
        let work = &self.runtime.cfg.work_dir;
        assert_eq!(
            std::fs::read(work.join("layers/sb1/upper/nested/it's data")).unwrap(),
            b"\0complete\xff"
        );
        assert_eq!(
            std::fs::read(work.join("snapshots/sb1/checkpoint")).unwrap(),
            b"snapshot"
        );
        assert!(work.join("bundles/sb1").is_dir());
    }

    fn assert_archive_files(&self, expected: &[&str]) {
        let archive = self.runtime.archive_path("sb1");
        let mut files: Vec<_> = std::fs::read_dir(archive.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        files.sort();
        assert_eq!(files, expected);
    }
}

fn shell(script: &str) -> Exec {
    let mut command = Exec::new("sh");
    command.args(["-c", script, "archive-test"]);
    command
}

#[test]
fn tar_failure_with_successful_zstd_preserves_source() {
    let fixture = Fixture::new();
    // A readable prefix followed by tar's read-error exit used to be accepted.
    let error = fixture
        .runtime
        .detach_with_commands(
            "sb1",
            &mut shell("printf partial; exit 2"),
            &mut Exec::new("zstd"),
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("tar"), "{error:#}");
    fixture.assert_source_preserved();
    fixture.assert_archive_files(&[]);
}

#[test]
fn zstd_failure_preserves_source() {
    let fixture = Fixture::new();
    let error = fixture
        .runtime
        .detach_with_commands(
            "sb1",
            &mut Exec::new("tar"),
            &mut shell("cat >/dev/null; printf partial; exit 42"),
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("zstd"), "{error:#}");
    fixture.assert_source_preserved();
    fixture.assert_archive_files(&[]);
}

#[test]
fn zstd_early_exit_does_not_leave_tar_blocked() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.runtime.cfg.work_dir.join("layers/sb1/upper/large"),
        vec![7; 1024 * 1024],
    )
    .unwrap();
    let error = fixture
        .runtime
        .detach_with_commands("sb1", &mut Exec::new("tar"), &mut shell("exit 42"))
        .unwrap_err();
    assert!(format!("{error:#}").contains("zstd"), "{error:#}");
    fixture.assert_source_preserved();
    fixture.assert_archive_files(&[]);
}

#[test]
fn missing_archive_programs_preserve_source() {
    for missing_tar in [true, false] {
        let fixture = Fixture::new();
        let missing = fixture.dir.path().join("nonexistent-program");
        let mut tar = Exec::new(if missing_tar {
            missing.as_os_str()
        } else {
            "tar".as_ref()
        });
        let mut zstd = Exec::new(if missing_tar {
            "zstd".as_ref()
        } else {
            missing.as_os_str()
        });
        let error = fixture
            .runtime
            .detach_with_commands("sb1", &mut tar, &mut zstd)
            .unwrap_err();
        let stage = if missing_tar { "tar" } else { "zstd" };
        assert!(format!("{error:#}").contains(stage), "{error:#}");
        fixture.assert_source_preserved();
        fixture.assert_archive_files(&[]);
    }
}

#[test]
fn failed_archive_does_not_replace_a_valid_archive() {
    let fixture = Fixture::new();
    fixture.runtime.detach("sb1").unwrap();
    let archive = fixture.runtime.archive_path("sb1");
    let original = std::fs::read(&archive).unwrap();
    fixture.seed();
    for (mut tar, mut zstd) in [
        (shell("printf partial; exit 2"), Exec::new("zstd")),
        (Exec::new("tar"), shell("cat >/dev/null; exit 42")),
    ] {
        assert!(fixture
            .runtime
            .detach_with_commands("sb1", &mut tar, &mut zstd)
            .is_err());
        assert_eq!(std::fs::read(&archive).unwrap(), original);
        fixture.assert_source_preserved();
        fixture.assert_archive_files(&["layer.tar.zst"]);
    }
}

#[test]
fn publication_failure_preserves_source_and_removes_temporary_archive() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.runtime.archive_path("sb1")).unwrap();
    let error = fixture.runtime.detach("sb1").unwrap_err();
    assert!(format!("{error:#}").contains("publishing"), "{error:#}");
    fixture.assert_source_preserved();
    fixture.assert_archive_files(&["layer.tar.zst"]);
    assert!(fixture.runtime.archive_path("sb1").is_dir());
}

#[test]
fn successful_detach_restores_all_contents_and_cleans_local_data() {
    let fixture = Fixture::new();
    fixture.runtime.detach("sb1").unwrap();
    fixture.assert_archive_files(&["layer.tar.zst"]);
    for directory in ["layers/sb1", "snapshots/sb1", "bundles/sb1"] {
        assert!(!fixture.runtime.cfg.work_dir.join(directory).exists());
    }
    let restored = fixture.dir.path().join("restored");
    std::fs::create_dir(&restored).unwrap();
    super::super::bundle::unpack_into(&fixture.runtime.archive_path("sb1"), &restored).unwrap();
    assert_eq!(
        std::fs::read(restored.join("nested/it's data")).unwrap(),
        b"\0complete\xff"
    );
    assert_eq!(
        std::fs::read(restored.join("other")).unwrap(),
        b"second file"
    );
}

fn set_overlay_attribute(path: &Path, name: &str) {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    let name = std::ffi::CString::new(name).unwrap();
    // Both C strings and the one-byte value remain valid throughout the call.
    let result =
        unsafe { libc::setxattr(path.as_ptr(), name.as_ptr(), b"y".as_ptr().cast(), 1, 0) };
    assert_eq!(
        result,
        0,
        "setting overlay attribute: {}",
        std::io::Error::last_os_error()
    );
}

#[test]
fn detach_preserves_user_overlay_opaque_attribute() {
    use std::os::unix::ffi::OsStrExt;
    let fixture = Fixture::new();
    let directory = fixture.runtime.cfg.work_dir.join("layers/sb1/upper/nested");
    set_overlay_attribute(&directory, "user.overlay.opaque");
    fixture.runtime.detach("sb1").unwrap();
    let restored = fixture.dir.path().join("restored upper's");
    std::fs::create_dir(&restored).unwrap();
    super::super::bundle::unpack_into(&fixture.runtime.archive_path("sb1"), &restored).unwrap();
    let path = std::ffi::CString::new(restored.join("nested").as_os_str().as_bytes()).unwrap();
    let mut value = [0u8; 1];
    // The destination buffer has the size passed to getxattr.
    let length = unsafe {
        libc::getxattr(
            path.as_ptr(),
            c"user.overlay.opaque".as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    assert_eq!(
        length,
        1,
        "reading restored attribute: {}",
        std::io::Error::last_os_error()
    );
    assert_eq!(value, *b"y");
}

struct OverlayFixture {
    fixture: Option<Fixture>,
    mounted: bool,
}

impl OverlayFixture {
    fn merged(&self) -> PathBuf {
        self.fixture.as_ref().unwrap().dir.path().join("merged")
    }

    fn mount(&mut self, userxattr: bool) {
        let fixture = self.fixture.as_ref().unwrap();
        let root = fixture.dir.path();
        let upper = fixture.runtime.cfg.work_dir.join("layers/sb1/upper");
        let options = format!(
            "lowerdir={},upperdir={},workdir={}{}",
            root.join("lower").display(),
            upper.display(),
            root.join("overlay-work").display(),
            if userxattr { ",userxattr" } else { "" }
        );
        let output = Exec::new("mount")
            .args(["-t", "overlay", "overlay", "-o", &options])
            .arg(self.merged())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "overlay mount failed (requires CAP_SYS_ADMIN): {}",
            String::from_utf8_lossy(&output.stderr)
        );
        self.mounted = true;
    }

    fn unmount(&mut self) {
        let status = Exec::new("umount").arg(self.merged()).status().unwrap();
        assert!(status.success(), "unmounting test overlay failed");
        self.mounted = false;
    }
}

impl Drop for OverlayFixture {
    fn drop(&mut self) {
        if self.mounted {
            let unmounted = Exec::new("umount")
                .arg(self.merged())
                .status()
                .is_ok_and(|status| status.success());
            if !unmounted {
                // Never recursively delete a mounted tree, even while unwinding a failed test.
                let fixture = self.fixture.take().unwrap();
                eprintln!(
                    "overlay cleanup failed; retained {}",
                    fixture.dir.keep().display()
                );
            }
        }
    }
}

fn overlay_detach_round_trip(userxattr: bool) {
    let fixture = Fixture::new();
    let root = fixture.dir.path();
    for directory in ["lower/deleted", "lower/replaced", "merged", "overlay-work"] {
        std::fs::create_dir_all(root.join(directory)).unwrap();
    }
    std::fs::write(root.join("lower/deleted/old"), b"deleted template data").unwrap();
    std::fs::write(root.join("lower/replaced/old"), b"hidden template data").unwrap();
    let mut overlay = OverlayFixture {
        fixture: Some(fixture),
        mounted: false,
    };
    overlay.mount(userxattr);
    std::fs::remove_dir_all(overlay.merged().join("deleted")).unwrap();
    std::fs::remove_dir_all(overlay.merged().join("replaced")).unwrap();
    std::fs::create_dir(overlay.merged().join("replaced")).unwrap();
    std::fs::write(overlay.merged().join("new"), b"new sandbox data").unwrap();
    assert!(!overlay.merged().join("replaced/old").exists());
    overlay.unmount();
    let fixture = overlay.fixture.as_ref().unwrap();
    fixture.runtime.detach("sb1").unwrap();
    let upper = fixture.runtime.cfg.work_dir.join("layers/sb1/upper");
    std::fs::create_dir_all(&upper).unwrap();
    super::super::bundle::unpack_into(&fixture.runtime.archive_path("sb1"), &upper).unwrap();
    overlay.mount(userxattr);
    assert!(!overlay.merged().join("deleted").exists());
    assert!(!overlay.merged().join("replaced/old").exists());
    assert_eq!(
        std::fs::read_dir(overlay.merged().join("replaced"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(
        std::fs::read(overlay.merged().join("new")).unwrap(),
        b"new sandbox data"
    );
    overlay.unmount();
}

#[test]
#[ignore = "requires Linux overlayfs and CAP_SYS_ADMIN; run explicitly as root"]
fn detach_overlay_trusted_round_trip() {
    overlay_detach_round_trip(false);
}

#[test]
#[ignore = "requires Linux overlayfs userxattr and CAP_SYS_ADMIN; run explicitly as root"]
fn detach_overlay_userxattr_round_trip() {
    overlay_detach_round_trip(true);
}

#[test]
fn detach_preserves_sparse_disk_contents() {
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::unix::fs::MetadataExt;
    let fixture = Fixture::new();
    let path = fixture
        .runtime
        .cfg
        .work_dir
        .join("layers/sb1/upper/disk.ext4");
    let mut disk = std::fs::File::create(path).unwrap();
    disk.write_all(b"disk header").unwrap();
    disk.seek(SeekFrom::Start(32 * 1024 * 1024)).unwrap();
    disk.write_all(b"disk tail").unwrap();
    drop(disk);
    fixture.runtime.detach("sb1").unwrap();
    let restored = fixture.dir.path().join("restored");
    std::fs::create_dir(&restored).unwrap();
    super::super::bundle::unpack_into(&fixture.runtime.archive_path("sb1"), &restored).unwrap();
    let mut disk = std::fs::File::open(restored.join("disk.ext4")).unwrap();
    let metadata = disk.metadata().unwrap();
    assert_eq!(metadata.len(), 32 * 1024 * 1024 + 9);
    assert!(
        metadata.blocks() * 512 < metadata.len() / 2,
        "restored disk must remain sparse"
    );
    let mut header = [0; 11];
    disk.read_exact(&mut header).unwrap();
    assert_eq!(&header, b"disk header");
    let mut hole = [1; 16];
    disk.seek(SeekFrom::Start(16 * 1024 * 1024)).unwrap();
    disk.read_exact(&mut hole).unwrap();
    assert_eq!(hole, [0; 16]);
    disk.seek(SeekFrom::Start(32 * 1024 * 1024)).unwrap();
    let mut tail = Vec::new();
    disk.read_to_end(&mut tail).unwrap();
    assert_eq!(tail, b"disk tail");
}

const SAVED_FILE: &str = "detached-only.txt";
const SAVED_CONTENT: &str = "written before detach, absent from the template\n";

/// Exercise real archive I/O and Firecracker prepare, without starting a guest.
/// The runsc fixture models the mount boundary so this test needs no mount privilege.
struct RestoreBackend {
    kind: BackendKind,
    firecracker: Firecracker,
    fail_boot: std::sync::atomic::AtomicBool,
    boot_attempts: std::sync::atomic::AtomicUsize,
    running: std::sync::atomic::AtomicBool,
}

impl RestoreBackend {
    fn read_saved_file(&self) -> Result<String> {
        match self.kind {
            BackendKind::Runsc => Ok(std::fs::read_to_string(
                self.firecracker
                    .work_dir
                    .join("layers/sb1/upper")
                    .join(SAVED_FILE),
            )?),
            BackendKind::Firecracker => debugfs(
                &self
                    .firecracker
                    .work_dir
                    .join("layers/sb1/disk/rootfs.ext4"),
                &format!("cat /{SAVED_FILE}"),
                false,
            ),
        }
    }
}

impl Backend for RestoreBackend {
    fn version(&self) -> String {
        "detached-restore-test".into()
    }

    fn template_root(&self, dir: &Path) -> PathBuf {
        match self.kind {
            BackendKind::Runsc => dir.join("rootfs"),
            BackendKind::Firecracker => self.firecracker.template_root(dir),
        }
    }

    fn prepare(&self, cfg: &RuntimeConfig, req: &bundle::BuildRequest<'_>) -> Result<PathBuf> {
        if self.kind == BackendKind::Firecracker {
            return self.firecracker.prepare(cfg, req);
        }
        let layer = cfg.work_dir.join("layers").join(req.id);
        let mounted = layer.join("mounted");
        if req.restore_from.is_some() && mounted.exists() {
            bail!(
                "{} already has a mounted layer; it cannot be restored over",
                req.id
            );
        }
        let upper = self.layer(cfg, req.id);
        std::fs::create_dir_all(&upper)?;
        if let Some(archive) = req.restore_from {
            bundle::unpack_into(archive, &upper)?;
        }
        std::fs::write(mounted, "")?;
        let bundle = cfg.work_dir.join("bundles").join(req.id);
        std::fs::create_dir_all(&bundle)?;
        std::fs::write(bundle.join("config.json"), "{}")?;
        Ok(bundle)
    }

    fn layer(&self, cfg: &RuntimeConfig, id: &str) -> PathBuf {
        match self.kind {
            BackendKind::Runsc => cfg.work_dir.join("layers").join(id).join("upper"),
            BackendKind::Firecracker => self.firecracker.layer(cfg, id),
        }
    }

    fn unmount(&self, cfg: &RuntimeConfig, id: &str) -> Result<()> {
        if self.kind == BackendKind::Firecracker {
            return self.firecracker.unmount(cfg, id);
        }
        let mounted = cfg.work_dir.join("layers").join(id).join("mounted");
        if mounted.exists() {
            std::fs::remove_file(mounted)?;
        }
        Ok(())
    }

    fn teardown(&self, cfg: &RuntimeConfig, id: &str) -> Result<()> {
        if self.kind == BackendKind::Firecracker {
            return self.firecracker.teardown(cfg, id);
        }
        for parent in ["layers", "bundles"] {
            let directory = cfg.work_dir.join(parent).join(id);
            if directory.exists() {
                std::fs::remove_dir_all(directory)?;
            }
        }
        Ok(())
    }

    fn boot(&self, _: &str, _: &Path) -> Result<()> {
        use std::sync::atomic::Ordering::Relaxed;
        self.boot_attempts.fetch_add(1, Relaxed);
        if self.fail_boot.swap(false, Relaxed) {
            bail!("injected detached boot failure");
        }
        self.running.store(true, Relaxed);
        Ok(())
    }

    fn restore(&self, _: &str, _: &Path, _: &Path) -> Result<()> {
        bail!("detached recovery must use Create, not checkpoint restore")
    }

    fn checkpoint(&self, _: &str, _: &Path, _: bool) -> Result<()> {
        bail!("checkpoint is not used by this fixture")
    }

    fn validate(&self, image: &Path) -> Result<()> {
        anyhow::ensure!(
            image.join("checkpoint.img").is_file(),
            "no checkpoint in this fixture"
        );
        Ok(())
    }

    fn remove(&self, _: &str) {
        self.running
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    fn kill(&self, id: &str) {
        self.remove(id);
    }

    fn exec(&self, _: &str, args: &[String], _: Duration) -> Result<(String, String, i64)> {
        anyhow::ensure!(
            self.running.load(std::sync::atomic::Ordering::Relaxed),
            "not booted"
        );
        assert_eq!(args, ["cat", &format!("/{SAVED_FILE}")]);
        Ok((self.read_saved_file()?, String::new(), 0))
    }
}

fn debugfs(disk: &Path, command: &str, writable: bool) -> Result<String> {
    let mut program = Exec::new("debugfs");
    if writable {
        program.arg("-w");
    }
    let output = program.args(["-R", command]).arg(disk).output()?;
    anyhow::ensure!(
        output.status.success(),
        "debugfs: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

struct RestoreFixture {
    _dir: TempDir,
    runtime: Runtime,
    backend: Arc<RestoreBackend>,
}

impl RestoreFixture {
    fn new(kind: BackendKind) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cfg = RuntimeConfig {
            backend: kind,
            work_dir: dir.path().join("work"),
            volume_root: dir.path().join("volumes"),
            ..Default::default()
        };
        let backend = Arc::new(RestoreBackend {
            kind,
            firecracker: Firecracker {
                bin: "unused-firecracker".into(),
                kernel: "/unused-kernel".into(),
                vcpus: 1,
                work_dir: cfg.work_dir.clone(),
                serial_console: false,
            },
            fail_boot: true.into(),
            boot_attempts: 0.into(),
            running: false.into(),
        });
        let runtime = Runtime::with_backend(cfg, backend.clone()).unwrap();
        let template = runtime.cfg.work_dir.join("templates/test");
        std::fs::create_dir_all(&template).unwrap();
        let layer = backend.layer(&runtime.cfg, "sb1");
        std::fs::create_dir_all(&layer).unwrap();
        match kind {
            BackendKind::Runsc => {
                std::fs::create_dir(template.join("rootfs")).unwrap();
                assert!(!template.join("rootfs").join(SAVED_FILE).exists());
                std::fs::write(layer.join(SAVED_FILE), SAVED_CONTENT).unwrap();
            }
            BackendKind::Firecracker => {
                let template_disk = template.join("rootfs.ext4");
                std::fs::File::create(&template_disk)
                    .unwrap()
                    .set_len(16 << 20)
                    .unwrap();
                let output = Exec::new("mke2fs")
                    .args(["-q", "-F", "-t", "ext4"])
                    .arg(&template_disk)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert!(
                    debugfs(&template_disk, &format!("cat /{SAVED_FILE}"), false)
                        .unwrap()
                        .is_empty()
                );
                let disk = layer.join("rootfs.ext4");
                std::fs::copy(&template_disk, &disk).unwrap();
                let source = dir.path().join("saved-content");
                std::fs::write(&source, SAVED_CONTENT).unwrap();
                debugfs(
                    &disk,
                    &format!("write {} /{SAVED_FILE}", source.display()),
                    true,
                )
                .unwrap();
            }
        }
        assert_eq!(backend.read_saved_file().unwrap(), SAVED_CONTENT);
        let bundle = runtime.cfg.work_dir.join("bundles/sb1");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("config.json"), "{}").unwrap();
        let workspace = runtime.cfg.volume_root.join("sb1/workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("user-data"), "user volume").unwrap();
        runtime.detach("sb1").unwrap();
        Self {
            _dir: dir,
            runtime,
            backend,
        }
    }

    fn create(&self) -> Result<()> {
        self.runtime.create(&CreateRequest {
            id: "sb1",
            template: "test",
            spec: &serde_json::json!({"args": ["app"], "ro_binds": []}),
            port: 8080,
            volume: None,
            net_index: None,
            from_tag: None,
            gateway_port: None,
            token: "",
            egress: &Default::default(),
        })
    }

    fn fail_boot(&self) {
        let error = self.create().unwrap_err();
        assert_eq!(error.to_string(), "injected detached boot failure");
        assert_eq!(
            self.backend
                .boot_attempts
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert_eq!(
            self.backend.read_saved_file().unwrap(),
            SAVED_CONTENT,
            "prepare completed before boot failed"
        );
    }

    fn assert_volume_preserved(&self) {
        assert_eq!(
            std::fs::read_to_string(self.runtime.cfg.volume_root.join("sb1/workspace/user-data"))
                .unwrap(),
            "user volume"
        );
    }
}

fn retry_detached_boot_failure(kind: BackendKind, reap_after_restart: bool) {
    let mut fixture = RestoreFixture::new(kind);
    let archive = fixture.runtime.archive_path("sb1");
    let saved_archive = std::fs::read(&archive).unwrap();
    fixture.fail_boot();
    assert_eq!(std::fs::read(&archive).unwrap(), saved_archive);
    if reap_after_restart {
        fixture.runtime =
            Runtime::with_backend(fixture.runtime.cfg.clone(), fixture.backend.clone()).unwrap();
        let bundle = fixture.runtime.cfg.work_dir.join("bundles/sb1");
        std::fs::File::open(&bundle)
            .unwrap()
            .set_modified(
                std::time::SystemTime::now() - super::super::DEAD_AFTER - Duration::from_secs(1),
            )
            .unwrap();
        assert_eq!(fixture.runtime.dead(super::super::DEAD_AFTER), ["sb1"]);
        let wake = super::super::Wakeups::new(|_, _, _| Ok(()));
        super::super::reap_dead(&fixture.runtime, &wake);
        for parent in ["layers", "bundles", "snapshots"] {
            assert!(!fixture
                .runtime
                .cfg
                .work_dir
                .join(parent)
                .join("sb1")
                .exists());
        }
        assert_eq!(std::fs::read(&archive).unwrap(), saved_archive);
        fixture.assert_volume_preserved();
    }
    fixture.create().unwrap();
    let (stdout, stderr, status) = fixture
        .runtime
        .exec(
            "sb1",
            &["cat".into(), format!("/{SAVED_FILE}")],
            Duration::from_secs(1),
        )
        .unwrap();
    assert_eq!(
        (stdout.as_str(), stderr.as_str(), status),
        (SAVED_CONTENT, "", 0)
    );
    assert_eq!(
        fixture
            .backend
            .boot_attempts
            .load(std::sync::atomic::Ordering::Relaxed),
        2
    );
    assert_eq!(std::fs::read(&archive).unwrap(), saved_archive);
}

#[test]
fn runsc_detached_boot_failure_can_retry_create() {
    retry_detached_boot_failure(BackendKind::Runsc, false);
}

#[test]
fn firecracker_detached_boot_failure_can_retry_create() {
    retry_detached_boot_failure(BackendKind::Firecracker, false);
}

#[test]
fn runsc_detached_boot_failure_survives_restart_and_reaping() {
    retry_detached_boot_failure(BackendKind::Runsc, true);
}

#[test]
fn firecracker_detached_boot_failure_survives_restart_and_reaping() {
    retry_detached_boot_failure(BackendKind::Firecracker, true);
}

#[test]
fn explicit_destroy_removes_failed_restore_archive_but_keeps_user_volume() {
    for kind in [BackendKind::Runsc, BackendKind::Firecracker] {
        let fixture = RestoreFixture::new(kind);
        fixture.fail_boot();
        fixture.runtime.destroy_with_net("sb1", None).unwrap();
        assert!(!fixture
            .runtime
            .archive_path("sb1")
            .parent()
            .unwrap()
            .exists());
        for parent in ["layers", "bundles", "snapshots"] {
            assert!(!fixture
                .runtime
                .cfg
                .work_dir
                .join(parent)
                .join("sb1")
                .exists());
        }
        fixture.assert_volume_preserved();
    }
}

#[test]
fn detached_create_does_not_replace_a_local_snapshot() {
    for kind in [BackendKind::Runsc, BackendKind::Firecracker] {
        let fixture = RestoreFixture::new(kind);
        fixture.fail_boot();
        let archive = fixture.runtime.archive_path("sb1");
        let saved_archive = std::fs::read(&archive).unwrap();
        let image = fixture.runtime.cfg.work_dir.join("snapshots/sb1");
        std::fs::create_dir_all(&image).unwrap();
        std::fs::write(image.join("checkpoint.img"), "saved memory").unwrap();

        assert!(fixture.create().is_err());

        assert_eq!(
            std::fs::read_to_string(image.join("checkpoint.img")).unwrap(),
            "saved memory"
        );
        assert_eq!(fixture.backend.read_saved_file().unwrap(), SAVED_CONTENT);
        assert_eq!(std::fs::read(archive).unwrap(), saved_archive);
        assert_eq!(
            fixture
                .backend
                .boot_attempts
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }
}

#[test]
fn detached_create_replaces_incomplete_local_layers_from_the_archive() {
    for kind in [BackendKind::Runsc, BackendKind::Firecracker] {
        let fixture = RestoreFixture::new(kind);
        let layer = fixture.backend.layer(&fixture.runtime.cfg, "sb1");
        std::fs::create_dir_all(&layer).unwrap();
        match kind {
            BackendKind::Runsc => {
                std::fs::write(layer.join(SAVED_FILE), "partial extraction").unwrap();
                std::fs::write(layer.parent().unwrap().join("mounted"), "").unwrap();
            }
            BackendKind::Firecracker => {
                std::fs::copy(
                    fixture
                        .runtime
                        .cfg
                        .work_dir
                        .join("templates/test/rootfs.ext4"),
                    layer.join("rootfs.ext4"),
                )
                .unwrap();
            }
        }
        fixture
            .backend
            .fail_boot
            .store(false, std::sync::atomic::Ordering::Relaxed);

        fixture.create().unwrap();

        let (stdout, stderr, status) = fixture
            .runtime
            .exec(
                "sb1",
                &["cat".into(), format!("/{SAVED_FILE}")],
                Duration::from_secs(1),
            )
            .unwrap();
        assert_eq!(
            (stdout.as_str(), stderr.as_str(), status),
            (SAVED_CONTENT, "", 0)
        );
    }
}
