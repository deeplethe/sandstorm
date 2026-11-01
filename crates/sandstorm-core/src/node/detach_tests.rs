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
    // Match bundle::unpack_into's existing restore pipeline and flags.
    let status = shell("zstd -dq -T0 -c \"$1\" | tar -C \"$2\" -xf -")
        .arg(fixture.runtime.archive_path("sb1"))
        .arg(&restored)
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        std::fs::read(restored.join("nested/it's data")).unwrap(),
        b"\0complete\xff"
    );
    assert_eq!(
        std::fs::read(restored.join("other")).unwrap(),
        b"second file"
    );
}
