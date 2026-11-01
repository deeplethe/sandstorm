//! Templates from container images.
//!
//! A node pulls the image from its registry itself, with nothing else
//! installed: no Docker, no skopeo. What it builds is what the backend reads,
//! a directory for runsc or an ext4 file for Firecracker, in `templates/`,
//! where an operator's own templates are.
//!
//! A pull takes minutes, so it runs on its own task. The command loop only
//! asks where it stands, as for a fetched tag, and the node reports that to
//! the master with its other observations.
//!
//! **An image is not trusted.** A layer is unpacked by root, so `tar` is not
//! used: it guards an archive against itself, not against the layer before.
//! One layer with `x -> /etc` and a later one with `x/passwd` would write the
//! host's. Every path here goes through `inside`, which reads the rootfs the
//! way a process chrooted into it would.

use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command as Exec, Stdio};
use std::sync::Arc;

use crate::proto::{ImageConfig, TemplatePull};

/// What a Firecracker guest boots into, and what it runs the program with.
const INIT: &str = include_str!("../../../../rootfs-init/sandstorm-init.sh");
const AGENT: &str = include_str!("../../../../rootfs-init/sandstorm-agent.py");

/// Far beyond any image meant for a sandbox: this is what stops a typo from
/// filling a node's disk.
const MAX_IMAGE_BYTES: u64 = 32 << 30;

/// Attempts at one thing from a registry, and the first wait between them.
const TRIES: u32 = 5;
const RETRY_PAUSE: std::time::Duration = std::time::Duration::from_secs(1);

/// Symbolic links followed resolving one path, as the kernel limits them.
const MAX_LINKS: u32 = 40;

const ACCEPT: &str = "application/vnd.oci.image.index.v1+json, \
    application/vnd.oci.image.manifest.v1+json, \
    application/vnd.docker.distribution.manifest.list.v2+json, \
    application/vnd.docker.distribution.manifest.v2+json";

// ---- naming an image -------------------------------------------------------

/// Where an image lives. What `docker pull` would make of the same string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub registry: String,
    pub repository: String,
    /// A tag or a digest.
    pub reference: String,
}

impl Reference {
    pub fn parse(image: &str) -> Result<Self> {
        let image = image.trim();
        if image.is_empty()
            || image.len() > 512
            || image.contains(|c: char| c.is_whitespace() || c.is_control())
            || image.contains("..")
        {
            bail!("{image:?} is not an image reference");
        }
        let (first, rest) = image.split_once('/').unwrap_or(("", image));
        let names_a_registry = first.contains('.') || first.contains(':') || first == "localhost";
        let (registry, path) = if names_a_registry {
            (first.to_string(), rest.to_string())
        } else {
            ("registry-1.docker.io".to_string(), image.to_string())
        };
        let registry = if registry == "docker.io" {
            "registry-1.docker.io".to_string()
        } else {
            registry
        };
        // A colon after the last slash is a tag; before it, a port.
        let (repository, reference) = if let Some((r, d)) = path.split_once('@') {
            (r.split(':').next().unwrap_or(r).to_string(), d.to_string())
        } else {
            match path.rsplit_once(':') {
                Some((r, t)) if !t.contains('/') => (r.to_string(), t.to_string()),
                _ => (path.clone(), "latest".to_string()),
            }
        };
        if repository.is_empty() || reference.is_empty() {
            bail!("{image:?} is not an image reference");
        }
        let repository = if registry == "registry-1.docker.io" && !repository.contains('/') {
            format!("library/{repository}")
        } else {
            repository
        };
        Ok(Self {
            registry,
            repository,
            reference,
        })
    }

    /// Plain HTTP only to this machine, as Docker treats a local registry.
    fn base(&self) -> String {
        let host = self.registry.split(':').next().unwrap_or("");
        let local = host == "localhost" || host == "127.0.0.1";
        format!(
            "{}://{}/v2/{}",
            if local { "http" } else { "https" },
            self.registry,
            self.repository
        )
    }
}

// ---- the registry ----------------------------------------------------------

struct Registry {
    http: reqwest::Client,
    base: String,
    bearer: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    #[serde(default)]
    manifests: Vec<Listed>,
    #[serde(default)]
    config: Option<Blob>,
    #[serde(default)]
    layers: Vec<Blob>,
}

#[derive(Deserialize)]
struct Listed {
    digest: String,
    #[serde(default)]
    platform: Option<Platform>,
}

#[derive(Deserialize)]
struct Platform {
    architecture: String,
    os: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Blob {
    digest: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    media_type: String,
}

#[derive(Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    config: Config,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Config {
    #[serde(default)]
    entrypoint: Option<Vec<String>>,
    #[serde(default)]
    cmd: Option<Vec<String>>,
    #[serde(default)]
    env: Option<Vec<String>>,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    exposed_ports: Option<HashMap<String, serde_json::Value>>,
}

impl From<Config> for ImageConfig {
    fn from(c: Config) -> Self {
        let mut ports: Vec<u16> = c
            .exposed_ports
            .into_iter()
            .flatten()
            .filter_map(|(p, _)| p.split('/').next()?.parse().ok())
            .collect();
        ports.sort_unstable();
        Self {
            args: [c.entrypoint.unwrap_or_default(), c.cmd.unwrap_or_default()].concat(),
            env: c.env.unwrap_or_default(),
            cwd: c.working_dir.filter(|d| !d.is_empty()),
            port: ports.first().copied(),
        }
    }
}

/// Whether this is the network failing, not the registry answering.
fn dropped(e: &anyhow::Error) -> bool {
    e.downcast_ref::<reqwest::Error>().is_some()
}

/// The value of `key="value"` in a `WWW-Authenticate` header.
fn challenge(header: &str, key: &str) -> Option<String> {
    let at = header.find(&format!("{key}=\""))? + key.len() + 2;
    Some(header[at..].split('"').next()?.to_string())
}

impl Registry {
    fn new(r: &Reference) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(15))
                .build()?,
            base: r.base(),
            bearer: None,
        })
    }

    /// A GET that takes the anonymous token a public registry asks for.
    async fn get(&mut self, path: &str) -> Result<reqwest::Response> {
        let url = format!("{}/{path}", self.base);
        let send = |bearer: Option<&str>| {
            let mut rb = self.http.get(&url).header("accept", ACCEPT);
            if let Some(t) = bearer {
                rb = rb.bearer_auth(t);
            }
            rb.send()
        };
        let mut res = send(self.bearer.as_deref()).await?;
        if res.status() == reqwest::StatusCode::UNAUTHORIZED {
            let header = res
                .headers()
                .get("www-authenticate")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            let Some(realm) = challenge(&header, "realm") else {
                bail!("{url} needs a login, and private registries are not supported");
            };
            let mut q = Vec::new();
            for key in ["service", "scope"] {
                if let Some(v) = challenge(&header, key) {
                    q.push((key, v));
                }
            }
            #[derive(Deserialize)]
            struct Token {
                #[serde(default)]
                token: String,
                #[serde(default)]
                access_token: String,
            }
            let got = self.http.get(&realm).query(&q).send().await?;
            if !got.status().is_success() {
                bail!("the registry refused an anonymous token: {}", got.status());
            }
            let t: Token = got.json().await?;
            let token = if t.token.is_empty() {
                t.access_token
            } else {
                t.token
            };
            res = send(Some(&token)).await?;
            self.bearer = Some(token);
        }
        if !res.status().is_success() {
            bail!("{url}: {}", res.status());
        }
        Ok(res)
    }

    /// A connection that drops says nothing about the image, and a minute of
    /// pulling should not be lost to one. Anything the registry itself said
    /// is an answer, and is not asked again.
    async fn manifest(&mut self, reference: &str) -> Result<Manifest> {
        let mut pause = RETRY_PAUSE;
        for _ in 1..TRIES {
            match self.manifest_once(reference).await {
                Err(e) if dropped(&e) => {
                    tracing::debug!(error = %e, "retrying a registry request");
                    tokio::time::sleep(pause).await;
                    pause *= 2;
                }
                out => return out,
            }
        }
        self.manifest_once(reference).await
    }

    async fn blob(&mut self, digest: &str, to: &Path) -> Result<()> {
        let mut pause = RETRY_PAUSE;
        for _ in 1..TRIES {
            match self.blob_once(digest, to).await {
                Err(e) if dropped(&e) => {
                    tracing::debug!(%digest, error = %e, "retrying a download");
                    tokio::time::sleep(pause).await;
                    pause *= 2;
                }
                out => return out,
            }
        }
        self.blob_once(digest, to).await
    }

    async fn manifest_once(&mut self, reference: &str) -> Result<Manifest> {
        let m: Manifest = self
            .get(&format!("manifests/{reference}"))
            .await?
            .json()
            .await?;
        if m.manifests.is_empty() {
            return Ok(m);
        }
        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => other,
        };
        let one = m
            .manifests
            .iter()
            .find(|l| {
                l.platform
                    .as_ref()
                    .is_some_and(|p| p.os == "linux" && p.architecture == arch)
            })
            .with_context(|| format!("the image has no linux/{arch} variant"))?
            .digest
            .clone();
        Ok(self.get(&format!("manifests/{one}")).await?.json().await?)
    }

    /// Into `to`, refused unless it hashes to its digest: a blob is named by
    /// its content, and a mirror or a proxy on the way must not change that.
    async fn blob_once(&mut self, digest: &str, to: &Path) -> Result<()> {
        use sha2::{Digest, Sha256};
        use std::io::Write;
        let Some(want) = digest.strip_prefix("sha256:") else {
            bail!("unsupported digest {digest}");
        };
        let mut res = self.get(&format!("blobs/{digest}")).await?;
        let mut file = std::io::BufWriter::new(std::fs::File::create(to)?);
        let mut sha = Sha256::new();
        let mut size = 0u64;
        while let Some(chunk) = res.chunk().await? {
            size += chunk.len() as u64;
            if size > MAX_IMAGE_BYTES {
                bail!(
                    "{digest} is larger than the {} GiB limit",
                    MAX_IMAGE_BYTES >> 30
                );
            }
            sha.update(&chunk);
            file.write_all(&chunk)?;
        }
        file.flush()?;
        if hex::encode(sha.finalize()) != want {
            bail!("{digest} did not match what was downloaded");
        }
        Ok(())
    }
}

// ---- unpacking -------------------------------------------------------------

/// `path` in `root`, as a process chrooted into `root` would find it: `..`
/// stops at the root, and a link, absolute or not, is followed inside it.
/// The last component is left alone when `follow_last` is false, which is how
/// a path that will be replaced is named. A component that is missing or not
/// a directory ends the walk: what comes after it does not exist yet.
fn inside(root: &Path, path: &Path, follow_last: bool) -> Result<PathBuf> {
    let mut todo: Vec<std::ffi::OsString> = Vec::new();
    let push = |todo: &mut Vec<std::ffi::OsString>, p: &Path| {
        for c in p.components().rev() {
            match c {
                Component::Normal(n) => todo.push(n.to_os_string()),
                Component::ParentDir => todo.push("..".into()),
                _ => {}
            }
        }
    };
    push(&mut todo, path);
    let mut at = PathBuf::new();
    let mut links = 0;
    while let Some(part) = todo.pop() {
        if part == ".." {
            at.pop();
            continue;
        }
        let here = root.join(&at).join(&part);
        let last = todo.is_empty();
        match here.symlink_metadata() {
            Ok(m) if m.file_type().is_symlink() && (!last || follow_last) => {
                links += 1;
                if links > MAX_LINKS {
                    bail!("too many links resolving {}", path.display());
                }
                let target = std::fs::read_link(&here)?;
                if target.is_absolute() {
                    at.clear();
                }
                push(&mut todo, &target);
            }
            _ => at.push(&part),
        }
    }
    Ok(root.join(at))
}

fn remove(path: &Path) -> Result<()> {
    match path.symlink_metadata() {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path)?,
        Ok(_) => std::fs::remove_file(path)?,
        Err(_) => {}
    }
    Ok(())
}

const WHITEOUT: &str = ".wh.";
const OPAQUE: &str = ".wh..wh..opq";

/// Lay one layer over `root`.
///
/// What a layer deletes it deletes from the layers under it, in no particular
/// order among its own entries, so deletions wait for the end of the layer
/// and skip what the layer itself wrote.
fn apply_layer<R: Read>(archive: R, root: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(archive);
    let mut written: HashSet<PathBuf> = HashSet::new();
    let mut gone: Vec<PathBuf> = Vec::new();
    let mut emptied: Vec<PathBuf> = Vec::new();
    // Set once the layer is done: a file's mode can forbid the writes that
    // fill it, and a parent's mtime moves as its children arrive.
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        let parent = path.parent().unwrap_or(Path::new(""));
        match name.as_deref() {
            Some(OPAQUE) => {
                emptied.push(inside(root, parent, true)?);
                continue;
            }
            Some(n) if n.starts_with(WHITEOUT) => {
                let target = inside(root, parent, true)?.join(&n[WHITEOUT.len()..]);
                gone.push(target);
                continue;
            }
            _ => {}
        }
        let kind = entry.header().entry_type();
        // Replaced, not followed: this names the entry itself.
        let dest = inside(root, &path, kind.is_dir())?;
        if dest == root {
            continue;
        }
        if let Some(p) = dest.parent() {
            std::fs::create_dir_all(p)?;
        }
        let (uid, gid) = (entry.header().uid()? as u32, entry.header().gid()? as u32);
        let mode = entry.header().mode()? & 0o7777;
        match kind {
            k if k.is_dir() => {
                if !dest.symlink_metadata().is_ok_and(|m| m.is_dir()) {
                    remove(&dest)?;
                    std::fs::create_dir(&dest)?;
                }
                std::os::unix::fs::chown(&dest, Some(uid), Some(gid)).ok();
                std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(mode))?;
            }
            k if k.is_symlink() => {
                let Some(target) = entry.link_name()? else {
                    bail!("a link with no target: {}", path.display());
                };
                remove(&dest)?;
                std::os::unix::fs::symlink(target.as_ref(), &dest)?;
                std::os::unix::fs::lchown(&dest, Some(uid), Some(gid)).ok();
            }
            k if k.is_hard_link() => {
                let Some(target) = entry.link_name()? else {
                    bail!("a link with no target: {}", path.display());
                };
                let from = inside(root, target.as_ref(), false)?;
                remove(&dest)?;
                std::fs::hard_link(&from, &dest)
                    .with_context(|| format!("linking {}", path.display()))?;
            }
            k if k.is_file() || k.is_gnu_sparse() => {
                remove(&dest)?;
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&dest)?;
                std::io::copy(&mut entry, &mut f)?;
                std::os::unix::fs::chown(&dest, Some(uid), Some(gid)).ok();
                // After the chown, which clears setuid.
                std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(mode))?;
            }
            // Devices and pipes: a sandbox's /dev is mounted over whatever
            // an image brings, and it has no business making any others.
            _ => continue,
        }
        written.insert(dest);
    }
    for dir in emptied {
        for child in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let p = child.path();
            if !written.iter().any(|w| w == &p || w.starts_with(&p)) {
                remove(&p)?;
            }
        }
    }
    for path in gone {
        if !written.contains(&path) {
            remove(&path)?;
        }
    }
    Ok(())
}

/// Decompress with what a node has already, `gzip` and `zstd`, in place of a
/// library each: it is the same work, and it runs outside this process.
fn unpack(blob: &Path, media_type: &str, root: &Path) -> Result<()> {
    let mut magic = [0u8; 4];
    let got = std::fs::File::open(blob)?.read(&mut magic)?;
    let tool = match (&magic[..got], media_type) {
        ([0x1f, 0x8b, ..], _) => Some("gzip"),
        ([0x28, 0xb5, 0x2f, 0xfd], _) => Some("zstd"),
        _ if media_type.ends_with("gzip") => Some("gzip"),
        _ => None,
    };
    let Some(tool) = tool else {
        return apply_layer(std::fs::File::open(blob)?, root);
    };
    let mut child = Exec::new(tool)
        .args(["-dc"])
        .stdin(std::fs::File::open(blob)?)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {tool}"))?;
    let out = child.stdout.take().context("the decompressor's stdout")?;
    let applied = apply_layer(out, root);
    let status = child.wait_with_output()?;
    applied?;
    if !status.status.success() {
        bail!("{tool}: {}", String::from_utf8_lossy(&status.stderr).trim());
    }
    Ok(())
}

// ---- turning it into a template --------------------------------------------

/// What a Firecracker guest needs of the image: its init is a bash script
/// that hands over to a Python agent, which starts the program.
fn check_bootable(rootfs: &Path) -> Result<()> {
    let has = |p: &str| inside(rootfs, Path::new(p), true).is_ok_and(|f| f.is_file());
    if !has("bin/bash") && !has("usr/bin/bash") {
        bail!("Firecracker needs bash in the image, and it has none");
    }
    let python = [
        "usr/local/bin/python3",
        "usr/bin/python3",
        "usr/local/bin/python",
        "usr/bin/python",
    ];
    if !python.iter().any(|p| has(p)) {
        bail!("Firecracker needs python3 in the image, and it has none");
    }
    Ok(())
}

/// Free room a sandbox's disk has beyond the image. The file is sparse and
/// each sandbox reflinks it, so this costs nothing until it is written.
const DISK_ROOM: u64 = 2 << 30;

fn tree_bytes(dir: &Path) -> u64 {
    let mut total = 0;
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let Ok(m) = e.path().symlink_metadata() else {
            continue;
        };
        total += m.blocks() * 512;
        if m.is_dir() {
            total += tree_bytes(&e.path());
        }
    }
    total
}

fn make_ext4(rootfs: &Path, to: &Path) -> Result<()> {
    std::fs::write(rootfs.join("sandstorm-init.sh"), INIT)?;
    std::fs::set_permissions(
        rootfs.join("sandstorm-init.sh"),
        std::fs::Permissions::from_mode(0o755),
    )?;
    std::fs::write(rootfs.join("sandstorm-agent.py"), AGENT)?;
    let size = tree_bytes(rootfs) / 4 * 5 + DISK_ROOM;
    std::fs::File::create(to)?.set_len(size)?;
    let out = Exec::new("mkfs.ext4")
        .arg("-q")
        .arg("-d")
        .arg(rootfs)
        .arg(to)
        .output()
        .context("running mkfs.ext4")?;
    if !out.status.success() {
        bail!("mkfs.ext4: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Prefix of a pull in progress. A leading dot is something a template name
/// may not have, so a half-built one is never taken for a template.
const PARTIAL: &str = ".pull-";

/// The file beside a built template that says what its image asks for.
const CONFIG: &str = "image.json";

async fn pull(
    templates: PathBuf,
    name: String,
    image: String,
    root_file: String,
) -> Result<ImageConfig> {
    let r = Reference::parse(&image)?;
    let mut reg = Registry::new(&r)?;
    let manifest = reg.manifest(&r.reference).await?;
    let config = manifest
        .config
        .as_ref()
        .context("the image has no config")?;
    let total: u64 = manifest.layers.iter().map(|l| l.size).sum();
    if total > MAX_IMAGE_BYTES {
        bail!(
            "{image} is {} GiB, over the {} GiB limit",
            total >> 30,
            MAX_IMAGE_BYTES >> 30
        );
    }

    let work = templates.join(format!("{PARTIAL}{name}"));
    let _ = std::fs::remove_dir_all(&work);
    let rootfs = work.join("rootfs");
    std::fs::create_dir_all(&rootfs)?;
    let done = async {
        let at = work.join("config");
        reg.blob(&config.digest, &at).await?;
        let file: ConfigFile =
            serde_json::from_slice(&std::fs::read(&at)?).context("reading the image's config")?;
        let parsed = ImageConfig::from(file.config);
        for layer in &manifest.layers {
            let at = work.join("layer");
            reg.blob(&layer.digest, &at).await?;
            let (blob, kind, root) = (at.clone(), layer.media_type.clone(), rootfs.clone());
            tokio::task::spawn_blocking(move || unpack(&blob, &kind, &root))
                .await?
                .with_context(|| format!("unpacking {}", layer.digest))?;
        }
        let (work2, rootfs2) = (work.clone(), rootfs.clone());
        let ext4 = root_file.ends_with(".ext4");
        tokio::task::spawn_blocking(move || -> Result<()> {
            // The sandbox mounts its own over these.
            for d in ["proc", "dev", "sys", "tmp"] {
                std::fs::create_dir_all(inside(&rootfs2, Path::new(d), true)?)?;
            }
            if ext4 {
                check_bootable(&rootfs2)?;
                make_ext4(&rootfs2, &work2.join("rootfs.ext4"))?;
            }
            Ok(())
        })
        .await??;
        Ok::<_, anyhow::Error>(parsed)
    }
    .await;
    let parsed = match done {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&work);
            return Err(e);
        }
    };

    let home = templates.join(&name);
    std::fs::create_dir_all(&home)?;
    std::fs::write(home.join(CONFIG), serde_json::to_vec(&parsed)?)?;
    std::fs::rename(work.join(&root_file), home.join(&root_file))?;
    let _ = std::fs::remove_dir_all(&work);
    Ok(parsed)
}

/// Pulls on this node, and the last word on each.
#[derive(Default)]
pub struct Pulls {
    state: Mutex<HashMap<String, TemplatePull>>,
}

impl Pulls {
    /// Make sure template `name` is on this node, starting a pull if it is
    /// not and none is running. `root` is the path the backend reads.
    ///
    /// Idempotent for one attempt, so a repeat of the command changes
    /// nothing; a new attempt starts again.
    pub fn ensure(self: &Arc<Self>, name: &str, image: &str, attempt: u32, root: &Path) {
        let mut st = self.state.lock();
        if st
            .get(name)
            .is_some_and(|p| p.attempt == attempt && p.image == image)
        {
            return;
        }
        let saved = root.parent().map(|d| d.join(CONFIG));
        if root.exists() {
            if let Some(config) = saved.and_then(|p| std::fs::read(p).ok()) {
                if let Ok(config) = serde_json::from_slice(&config) {
                    st.insert(
                        name.to_string(),
                        TemplatePull::done(name, image, attempt, config),
                    );
                    return;
                }
            }
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        st.insert(
            name.to_string(),
            TemplatePull::running(name, image, attempt),
        );
        drop(st);

        let Some(home) = root.parent().and_then(Path::parent).map(Path::to_path_buf) else {
            return;
        };
        let root_file = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (me, name, image) = (self.clone(), name.to_string(), image.to_string());
        rt.spawn(async move {
            let t0 = std::time::Instant::now();
            let out = pull(home, name.clone(), image.clone(), root_file).await;
            let secs = t0.elapsed().as_secs();
            let report = match out {
                Ok(c) => {
                    tracing::info!(template = %name, %image, secs, "pulled an image");
                    TemplatePull::done(&name, &image, attempt, c)
                }
                Err(e) => {
                    let why = format!("{e:#}");
                    tracing::warn!(template = %name, %image, error = %why, "could not pull an image");
                    TemplatePull::failed(&name, &image, attempt, why)
                }
            };
            me.state.lock().insert(name, report);
        });
    }

    /// A template this node can never hold under that name.
    pub fn refuse(&self, name: &str, image: &str, attempt: u32, why: String) {
        self.state.lock().insert(
            name.to_string(),
            TemplatePull::failed(name, image, attempt, why),
        );
    }

    /// Whether one is running: the node comes back sooner while it is, or
    /// the end of a pull waits out a long poll before anyone hears of it.
    pub fn busy(&self) -> bool {
        self.state.lock().values().any(|p| !p.done)
    }

    /// Where every pull stands, for the next report.
    pub fn report(&self) -> Vec<TemplatePull> {
        self.state.lock().values().cloned().collect()
    }
}

#[cfg(test)]
#[path = "images_tests.rs"]
mod tests;
