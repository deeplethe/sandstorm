use super::*;
use std::os::unix::fs::symlink;
use tempfile::TempDir;

/// A layer, from entries: ("path", Some("contents")) for a file, ("path/", None)
/// for a directory, and ("path", None) with "-> target" for a link.
fn layer(entries: &[(&str, &str)]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for (path, body) in entries {
        let mut h = tar::Header::new_gnu();
        h.set_mode(0o644);
        h.set_uid(0);
        h.set_gid(0);
        if let Some(target) = body.strip_prefix("-> ") {
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_size(0);
            b.append_link(&mut h, path, target).unwrap();
        } else if let Some(target) = body.strip_prefix("=> ") {
            h.set_entry_type(tar::EntryType::Link);
            h.set_size(0);
            b.append_link(&mut h, path, target).unwrap();
        } else if path.ends_with('/') {
            h.set_entry_type(tar::EntryType::Directory);
            h.set_mode(0o755);
            h.set_size(0);
            b.append_data(&mut h, path, &[][..]).unwrap();
        } else {
            h.set_size(body.len() as u64);
            b.append_data(&mut h, path, body.as_bytes()).unwrap();
        }
    }
    b.into_inner().unwrap()
}

fn rootfs() -> (TempDir, PathBuf) {
    let td = TempDir::new().unwrap();
    let root = td.path().join("root");
    std::fs::create_dir(&root).unwrap();
    (td, root)
}

fn apply(entries: &[(&str, &str)], root: &Path) {
    apply_layer(&layer(entries)[..], root).unwrap();
}

/// The one this module exists for: a link one layer left, used by the next.
#[test]
fn a_link_from_an_earlier_layer_does_not_lead_out_of_the_rootfs() {
    let (td, root) = rootfs();
    let outside = td.path().join("host");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("passwd"), "root").unwrap();

    apply(&[("x", &format!("-> {}", outside.display()))], &root);
    apply(&[("x/passwd", "owned")], &root);

    assert_eq!(
        std::fs::read_to_string(outside.join("passwd")).unwrap(),
        "root"
    );
    let inside_copy = root.join(outside.strip_prefix("/").unwrap()).join("passwd");
    assert_eq!(std::fs::read_to_string(inside_copy).unwrap(), "owned");
}

#[test]
fn a_relative_link_cannot_climb_out_either() {
    let (td, root) = rootfs();
    apply(&[("x", "-> ../../.."), ("d/", "")], &root);
    apply(&[("x/escaped", "owned")], &root);
    assert!(!td.path().join("escaped").exists());
    assert!(!td.path().parent().unwrap().join("escaped").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("escaped")).unwrap(),
        "owned"
    );
}

#[test]
fn a_dotted_path_in_an_entry_stays_inside() {
    let (td, root) = rootfs();
    // The builder refuses to write these, so write the header by hand.
    let mut h = tar::Header::new_gnu();
    let name = b"../escaped";
    h.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name);
    h.set_size(5);
    h.set_mode(0o644);
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(0);
    h.set_cksum();
    let mut raw = h.as_bytes().to_vec();
    raw.extend_from_slice(b"owned");
    raw.resize(1024, 0);
    raw.resize(3072, 0);
    apply_layer(&raw[..], &root).unwrap();
    assert!(!td.path().join("escaped").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("escaped")).unwrap(),
        "owned"
    );
}

#[test]
fn a_whiteout_through_a_link_deletes_nothing_outside() {
    let (td, root) = rootfs();
    let outside = td.path().join("host");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("keep"), "x").unwrap();
    apply(&[("x", &format!("-> {}", outside.display()))], &root);
    apply(&[("x/.wh.keep", "")], &root);
    apply(&[("y", &format!("-> {}", outside.display()))], &root);
    apply(&[("y/.wh..wh..opq", "")], &root);
    assert!(outside.join("keep").exists());
}

#[test]
fn a_hard_link_cannot_reach_a_file_outside() {
    let (td, root) = rootfs();
    let secret = td.path().join("secret");
    std::fs::write(&secret, "s").unwrap();
    apply(&[("x", &format!("-> {}", td.path().display()))], &root);
    let hard = apply_layer(&layer(&[("h", "=> x/secret")])[..], &root);
    assert!(hard.is_err(), "there is no such file inside the rootfs");
    assert!(!root.join("h").exists());
}

#[test]
fn a_link_in_the_rootfs_is_kept_as_a_link() {
    let (_td, root) = rootfs();
    apply(&[("usr/lib/x", "1"), ("lib", "-> usr/lib")], &root);
    assert!(root
        .join("lib")
        .symlink_metadata()
        .unwrap()
        .file_type()
        .is_symlink());
    // A later layer writing through it lands where the link points.
    apply(&[("lib/y", "2")], &root);
    assert_eq!(
        std::fs::read_to_string(root.join("usr/lib/y")).unwrap(),
        "2"
    );
}

#[test]
fn a_whiteout_removes_a_lower_file() {
    let (_td, root) = rootfs();
    apply(&[("a/b", "1"), ("a/c", "2")], &root);
    apply(&[("a/.wh.b", "")], &root);
    assert!(!root.join("a/b").exists());
    assert!(root.join("a/c").exists());
}

/// In either order: whiteouts belong to the layers underneath.
#[test]
fn a_whiteout_leaves_what_its_own_layer_wrote() {
    let (_td, root) = rootfs();
    apply(&[("a/b", "1")], &root);
    apply(&[("a/b", "2"), ("a/.wh.b", "")], &root);
    assert_eq!(std::fs::read_to_string(root.join("a/b")).unwrap(), "2");
}

#[test]
fn an_opaque_directory_hides_only_what_is_below_it() {
    let (_td, root) = rootfs();
    apply(&[("a/old", "1")], &root);
    apply(&[("a/new", "2"), ("a/.wh..wh..opq", "")], &root);
    assert!(!root.join("a/old").exists());
    assert_eq!(std::fs::read_to_string(root.join("a/new")).unwrap(), "2");
}

#[test]
fn a_file_replaces_a_directory_and_the_reverse() {
    let (_td, root) = rootfs();
    apply(&[("a/b", "1"), ("c", "2")], &root);
    apply(&[("a", "3"), ("c/", "")], &root);
    assert_eq!(std::fs::read_to_string(root.join("a")).unwrap(), "3");
    assert!(root.join("c").is_dir());
}

#[test]
fn a_loop_of_links_is_an_error_not_a_hang() {
    let (_td, root) = rootfs();
    symlink("b", root.join("a")).unwrap();
    symlink("a", root.join("b")).unwrap();
    assert!(inside(&root, Path::new("a/x"), true).is_err());
}

#[test]
fn references_read_as_docker_reads_them() {
    let r = |s: &str| {
        let p = Reference::parse(s).unwrap();
        (p.registry, p.repository, p.reference)
    };
    let hub = "registry-1.docker.io".to_string();
    assert_eq!(
        r("python"),
        (hub.clone(), "library/python".into(), "latest".into())
    );
    assert_eq!(
        r("python:3.12"),
        (hub.clone(), "library/python".into(), "3.12".into())
    );
    assert_eq!(
        r("e2bdev/base:1"),
        (hub.clone(), "e2bdev/base".into(), "1".into())
    );
    assert_eq!(
        r("docker.io/library/ubuntu"),
        (hub, "library/ubuntu".into(), "latest".into())
    );
    assert_eq!(
        r("127.0.0.1:5000/sst-apps:1"),
        ("127.0.0.1:5000".into(), "sst-apps".into(), "1".into())
    );
    assert_eq!(
        r("ghcr.io/a/b@sha256:abc"),
        ("ghcr.io".into(), "a/b".into(), "sha256:abc".into())
    );
    assert_eq!(
        r("localhost:5000/x"),
        ("localhost:5000".into(), "x".into(), "latest".into())
    );
    for bad in ["", "a b", "x/../y", "a\nb"] {
        assert!(Reference::parse(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn only_this_machine_is_reached_over_plain_http() {
    let base = |s: &str| Reference::parse(s).unwrap().base();
    assert_eq!(base("127.0.0.1:5000/x"), "http://127.0.0.1:5000/v2/x");
    assert_eq!(base("localhost:5000/x"), "http://localhost:5000/v2/x");
    assert_eq!(base("ghcr.io/a/b"), "https://ghcr.io/v2/a/b");
    assert_eq!(
        base("evil.localhost.example/x"),
        "https://evil.localhost.example/v2/x"
    );
}

#[test]
fn the_image_config_becomes_how_to_run_it() {
    let c: ConfigFile = serde_json::from_value(serde_json::json!({
        "config": {
            "Entrypoint": ["python3"],
            "Cmd": ["app.py"],
            "Env": ["A=1"],
            "WorkingDir": "/app",
            "ExposedPorts": {"9000/tcp": {}, "8000/tcp": {}}
        }
    }))
    .unwrap();
    let c = ImageConfig::from(c.config);
    assert_eq!(c.args, ["python3", "app.py"]);
    assert_eq!(c.env, ["A=1"]);
    assert_eq!(c.cwd.as_deref(), Some("/app"));
    assert_eq!(
        c.port,
        Some(8000),
        "the lowest, so it does not depend on hash order"
    );
}

#[test]
fn the_challenge_is_read_as_a_registry_sends_it() {
    let h = r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/python:pull""#;
    assert_eq!(
        challenge(h, "realm").unwrap(),
        "https://auth.docker.io/token"
    );
    assert_eq!(challenge(h, "service").unwrap(), "registry.docker.io");
    assert_eq!(
        challenge(h, "scope").unwrap(),
        "repository:library/python:pull"
    );
}

#[test]
fn firecracker_says_what_an_image_lacks() {
    let (_td, root) = rootfs();
    apply(&[("bin/sh", "x")], &root);
    assert!(check_bootable(&root)
        .unwrap_err()
        .to_string()
        .contains("bash"));
    apply(&[("bin/bash", "x")], &root);
    assert!(check_bootable(&root)
        .unwrap_err()
        .to_string()
        .contains("python3"));
    apply(&[("usr/bin/python3", "x")], &root);
    check_bootable(&root).unwrap();
}
