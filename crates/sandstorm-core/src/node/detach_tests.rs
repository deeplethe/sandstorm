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
