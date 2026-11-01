//! Snapshot tags moving between nodes.
//!
//! A tag's image is written on the node that branched it. A fork can run on
//! any node in the tag's pool -- a runsc checkpoint restores on another
//! machine with the same fingerprint, measured 8 of 8 on 2026-10-27 -- so a
//! node asked to fork a tag it does not have fetches the image from the node
//! that does, keeps it, and restores from its own copy from then on.
//!
//! Both halves live here: the routes a listening node serves the image on,
//! and the fetch on the other end. The credential between them is a token
//! for one tag and one fetching node (`tag::fetch_token`), never a pairing
//! code.

use anyhow::{bail, Context, Result};
use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::proto::FetchFrom;

/// Headers a fetch carries. The token rides in a header, not the URL, so it
/// stays out of any access log on the way.
const FOR_NODE: &str = "x-sandstorm-fetch-for";
const EXPIRES: &str = "x-sandstorm-fetch-expires";
const TOKEN: &str = "x-sandstorm-fetch-token";

/// A tag younger than this is not reported as held. A branch writes the
/// image before the master records the tag, and a tag the master has no
/// record of is one it drops, so a fresh one must not be seen yet.
pub const REPORT_MIN_AGE: Duration = Duration::from_secs(300);

/// Prefix of a fetch in progress. A leading dot is something `tag::is_safe`
/// refuses, so a half-written copy can never be taken for a tag.
const PARTIAL: &str = ".fetch-";

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub files: Vec<ManifestFile>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ManifestFile {
    pub name: String,
    pub size: u64,
}

// ---- serving ---------------------------------------------------------------

struct Serve {
    tags_dir: PathBuf,
    secret: String,
}

/// The routes a listening node serves its tags on. Merged into the agent's
/// router, so they share its port and its certificate.
pub fn routes(tags_dir: PathBuf, secret: String) -> Router {
    Router::new()
        .route("/v1/agent/tags/:tag", get(manifest))
        .route("/v1/agent/tags/:tag/files/:file", get(file))
        .with_state(Arc::new(Serve { tags_dir, secret }))
}

type Refusal = (StatusCode, Json<serde_json::Value>);

fn refuse(status: StatusCode, msg: impl std::fmt::Display) -> Refusal {
    (
        status,
        Json(serde_json::json!({ "error": msg.to_string() })),
    )
}

fn authorised(s: &Serve, tag: &str, h: &HeaderMap) -> Result<(), Refusal> {
    let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).unwrap_or("");
    let expires = get(EXPIRES).parse::<i64>().unwrap_or(0);
    if crate::tag::fetch_token_valid(
        &s.secret,
        tag,
        get(FOR_NODE),
        expires,
        get(TOKEN),
        crate::state::now_secs(),
    ) {
        Ok(())
    } else {
        Err(refuse(
            StatusCode::UNAUTHORIZED,
            "fetching a tag needs a token for this tag, this node and a time that has not passed",
        ))
    }
}

/// The tag's directory, if the name is plain and the image is here.
fn tag_dir(s: &Serve, tag: &str) -> Result<PathBuf, Refusal> {
    if !crate::tag::is_safe(tag) {
        return Err(refuse(StatusCode::BAD_REQUEST, "not a plain tag name"));
    }
    let dir = s.tags_dir.join(tag);
    if !dir.is_dir() {
        return Err(refuse(
            StatusCode::NOT_FOUND,
            format!("tag {tag} is not on this node"),
        ));
    }
    Ok(dir)
}

/// What the image is made of. Flat files only: that is what runsc writes,
/// and a layout with anything else in it is refused rather than half-copied.
pub fn list_image(dir: &Path) -> Result<Vec<ManifestFile>> {
    let mut files = Vec::new();
    for e in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        // A backend's local cache beside the image, such as Firecracker's
        // assembled memory: derived from the files here, never sent.
        if name.starts_with('.') {
            continue;
        }
        let meta = e.metadata()?;
        if !meta.is_file() {
            bail!("{} holds {name}, which is not a plain file", dir.display());
        }
        files.push(ManifestFile {
            name,
            size: meta.len(),
        });
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(files)
}

async fn manifest(
    State(s): State<Arc<Serve>>,
    UrlPath(tag): UrlPath<String>,
    h: HeaderMap,
) -> Result<Json<Manifest>, Refusal> {
    authorised(&s, &tag, &h)?;
    let dir = tag_dir(&s, &tag)?;
    let files = list_image(&dir)
        .map_err(|e| refuse(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(Json(Manifest { files }))
}

async fn file(
    State(s): State<Arc<Serve>>,
    UrlPath((tag, name)): UrlPath<(String, String)>,
    h: HeaderMap,
) -> Result<Response, Refusal> {
    authorised(&s, &tag, &h)?;
    let dir = tag_dir(&s, &tag)?;
    // The same rule as a tag: one plain name inside this one directory.
    if !crate::tag::is_safe(&name) {
        return Err(refuse(StatusCode::BAD_REQUEST, "not a plain file name"));
    }
    let path = dir.join(&name);
    let meta = tokio::fs::symlink_metadata(&path)
        .await
        .map_err(|_| refuse(StatusCode::NOT_FOUND, format!("{tag} has no {name}")))?;
    if !meta.is_file() {
        return Err(refuse(
            StatusCode::NOT_FOUND,
            format!("{tag} has no file {name}"),
        ));
    }
    let f = tokio::fs::File::open(&path)
        .await
        .map_err(|e| refuse(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    // Streamed: an image is as large as the sandbox's memory, and a node
    // with 2 GB must not read a 1 GiB image into it to send it.
    let body = Body::from_stream(tokio_util::io::ReaderStream::new(f));
    Ok(([("content-length", meta.len().to_string())], body).into_response())
}

// ---- fetching ----------------------------------------------------------------

/// Fetches in progress on this node, by tag, so that many children of one
/// tag arriving together cause one fetch.
#[derive(Default)]
pub struct Fetches {
    state: Mutex<HashMap<String, Fetch>>,
}

enum Fetch {
    Running,
    Failed(String),
}

/// Where a tag stands on this node.
#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    /// The image is here.
    Ready,
    /// Being fetched; ask again shortly.
    Pending,
    /// The last fetch failed. Reported once; the next ask starts again.
    Failed(String),
}

impl Fetches {
    /// Make sure `tag`'s image is in `tags_dir`, starting a fetch from
    /// `from` if it is not and none is running.
    ///
    /// Called from the command loop, which must not wait on the network, so
    /// the fetch runs on its own task and this only reports where it is.
    pub fn ensure(self: &Arc<Self>, tags_dir: &Path, tag: &str, from: &FetchFrom) -> Status {
        if tags_dir.join(tag).is_dir() {
            return Status::Ready;
        }
        let mut st = self.state.lock();
        match st.get(tag) {
            Some(Fetch::Running) => return Status::Pending,
            Some(Fetch::Failed(e)) => {
                let e = e.clone();
                st.remove(tag);
                return Status::Failed(e);
            }
            None => {}
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return Status::Failed("no async runtime to fetch on".into());
        };
        st.insert(tag.to_string(), Fetch::Running);
        drop(st);

        let (me, dir, tag, from) = (
            self.clone(),
            tags_dir.to_path_buf(),
            tag.to_string(),
            from.clone(),
        );
        rt.spawn(async move {
            let t0 = std::time::Instant::now();
            let out = fetch(&dir, &tag, &from).await;
            let mut st = me.state.lock();
            match out {
                Ok(bytes) => {
                    tracing::info!(
                        %tag, from = %from.node_id, mib = bytes >> 20,
                        ms = t0.elapsed().as_millis() as u64, "fetched a tag"
                    );
                    st.remove(&tag);
                }
                Err(e) => {
                    let e = format!("fetching tag {tag} from {}: {e:#}", from.node_id);
                    tracing::warn!(%tag, error = %e, "tag fetch failed");
                    st.insert(tag, Fetch::Failed(e));
                }
            }
        });
        Status::Pending
    }
}

/// Copy a tag's image from the node in `from` into `tags_dir/tag`.
///
/// Into a hidden directory first, renamed into place only once every file
/// is complete, so a restore never sees half an image and a crash leaves
/// nothing that looks like a tag.
pub async fn fetch(tags_dir: &Path, tag: &str, from: &FetchFrom) -> Result<u64> {
    if !crate::tag::is_safe(tag) {
        bail!("tag {tag:?} is not a plain name");
    }
    let client = reqwest::Client::builder()
        .use_preconfigured_tls(crate::tls::pinned_leaf_client_config(&from.cert_sha256))
        .connect_timeout(Duration::from_secs(10))
        .build()?;
    let base = {
        let mut u = reqwest::Url::parse(&format!("https://{}/", from.address))
            .with_context(|| format!("{} is not an address", from.address))?;
        u.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("{} cannot be a base URL", from.address))?
            .pop_if_empty()
            .extend(["v1", "agent", "tags", tag]);
        u
    };
    let authed = |rb: reqwest::RequestBuilder| {
        rb.header(FOR_NODE, &from.for_node)
            .header(EXPIRES, from.expires.to_string())
            .header(TOKEN, &from.token)
    };

    let r = authed(client.get(base.clone()))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .with_context(|| format!("asking {} for the image", from.address))?;
    if !r.status().is_success() {
        bail!(
            "{} answered {}: {}",
            from.address,
            r.status(),
            r.text().await.unwrap_or_default()
        );
    }
    let manifest: Manifest = r.json().await?;
    if manifest.files.is_empty() {
        bail!("{} lists no files for {tag}", from.address);
    }

    std::fs::create_dir_all(tags_dir)?;
    let tmp = tags_dir.join(format!(
        "{PARTIAL}{tag}-{}",
        crate::identity::random_token()
    ));
    std::fs::create_dir_all(&tmp)?;
    let result = async {
        let mut total = 0u64;
        for f in &manifest.files {
            if !crate::tag::is_safe(&f.name) {
                bail!("{} named a file {:?}", from.address, f.name);
            }
            let mut u = base.clone();
            u.path_segments_mut()
                .expect("an https URL has segments")
                .extend(["files", f.name.as_str()]);
            // No overall timeout: an image is as large as a sandbox's memory.
            // A stalled transfer still fails, on the read timeout.
            let mut r = authed(client.get(u))
                .send()
                .await
                .with_context(|| format!("fetching {}", f.name))?;
            if !r.status().is_success() {
                bail!("{} answered {} for {}", from.address, r.status(), f.name);
            }
            let mut out = tokio::fs::File::create(tmp.join(&f.name)).await?;
            let mut got = 0u64;
            use tokio::io::AsyncWriteExt;
            while let Some(chunk) = tokio::time::timeout(Duration::from_secs(30), r.chunk())
                .await
                .with_context(|| format!("{} stalled", f.name))??
            {
                got += chunk.len() as u64;
                out.write_all(&chunk).await?;
            }
            out.sync_all().await?;
            if got != f.size {
                bail!(
                    "{} arrived as {got} bytes; the manifest said {}",
                    f.name,
                    f.size
                );
            }
            total += got;
        }
        Ok(total)
    }
    .await;

    match result {
        Ok(total) => {
            let dest = tags_dir.join(tag);
            match std::fs::rename(&tmp, &dest) {
                Ok(()) => Ok(total),
                // Another fetch of the same tag finished first; its copy is
                // as good as this one.
                Err(_) if dest.is_dir() => {
                    std::fs::remove_dir_all(&tmp).ok();
                    Ok(total)
                }
                Err(e) => {
                    std::fs::remove_dir_all(&tmp).ok();
                    Err(e).with_context(|| format!("moving {tag} into place"))
                }
            }
        }
        Err(e) => {
            std::fs::remove_dir_all(&tmp).ok();
            Err(e)
        }
    }
}

// ---- what this node holds ------------------------------------------------------

/// Tags whose images are in `tags_dir` and at least `min_age` old.
pub fn held(tags_dir: &Path, min_age: Duration) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(tags_dir) else {
        return Vec::new();
    };
    let now = SystemTime::now();
    let mut out: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if !crate::tag::is_safe(&name) {
                return None;
            }
            let modified = e.metadata().ok()?.modified().ok()?;
            (now.duration_since(modified).unwrap_or_default() >= min_age).then_some(name)
        })
        .collect();
    out.sort();
    out
}

/// Remove fetches a previous run of this node left half done.
pub fn clear_partial(tags_dir: &Path) {
    let Ok(rd) = std::fs::read_dir(tags_dir) else {
        return;
    };
    for e in rd.filter_map(|e| e.ok()) {
        if e.file_name().to_string_lossy().starts_with(PARTIAL) {
            tracing::info!(path = %e.path().display(), "removing an unfinished tag fetch");
            std::fs::remove_dir_all(e.path()).ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A node serving `tags_dir` over TLS with a real agent identity, as a
    /// listening node does. Returns its address and identity.
    async fn source(tags_dir: PathBuf, pki: &Path) -> (String, super::super::AgentIdentity) {
        let id = super::super::AgentIdentity::load_or_create(pki).unwrap();
        let app = routes(tags_dir, id.secret.clone());
        let tls = crate::tls::plain_server_config(&id.cert_pem, &id.key_pem).unwrap();
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.set_nonblocking(true).unwrap();
        let addr = l.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            axum_server::from_tcp_rustls(
                l,
                axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(tls)),
            )
            .unwrap()
            .serve(app.into_make_service())
            .await
            .ok();
        });
        (addr, id)
    }

    fn from(addr: &str, id: &super::super::AgentIdentity, tag: &str, for_node: &str) -> FetchFrom {
        let expires = crate::tag::fetch_expiry(crate::state::now_secs());
        FetchFrom {
            node_id: "src".into(),
            address: addr.into(),
            cert_sha256: id.fingerprint.clone(),
            for_node: for_node.into(),
            expires,
            token: crate::tag::fetch_token(&id.secret, tag, for_node, expires),
        }
    }

    fn image(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("checkpoint.img"), b"state").unwrap();
        // Bigger than one chunk, so the copy has to stream.
        let pages: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.join("pages.img"), pages).unwrap();
    }

    #[tokio::test]
    async fn a_tag_crosses_to_another_node_intact() {
        let (src, dst) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        image(&src.path().join("tags/py-base"));
        let (addr, id) = source(src.path().join("tags"), &src.path().join("pki")).await;

        let tags = dst.path().join("tags");
        let bytes = fetch(&tags, "py-base", &from(&addr, &id, "py-base", "f5"))
            .await
            .unwrap();
        assert_eq!(bytes, 3_000_005);
        for f in ["checkpoint.img", "pages.img"] {
            assert_eq!(
                std::fs::read(tags.join("py-base").join(f)).unwrap(),
                std::fs::read(src.path().join("tags/py-base").join(f)).unwrap(),
                "{f} differs"
            );
        }
        // Nothing half-written is left beside it.
        assert_eq!(held(&tags, Duration::ZERO), vec!["py-base".to_string()]);
        assert_eq!(std::fs::read_dir(&tags).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn a_token_for_something_else_gets_nothing() {
        let (src, dst) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        image(&src.path().join("tags/py-base"));
        image(&src.path().join("tags/secret-tag"));
        let (addr, id) = source(src.path().join("tags"), &src.path().join("pki")).await;
        let tags = dst.path().join("tags");

        // A token for one tag does not open another.
        let mut f = from(&addr, &id, "py-base", "f5");
        let e = fetch(&tags, "secret-tag", &f).await.unwrap_err();
        assert!(format!("{e:#}").contains("401"), "{e:#}");

        // Nor does one minted for another node, or with another secret.
        f = from(&addr, &id, "py-base", "f7");
        f.for_node = "f5".into();
        assert!(fetch(&tags, "py-base", &f).await.is_err());
        f = from(&addr, &id, "py-base", "f5");
        f.token = crate::tag::fetch_token("not-the-secret", "py-base", "f5", f.expires);
        assert!(fetch(&tags, "py-base", &f).await.is_err());

        // And a server presenting another certificate is not talked to.
        f = from(&addr, &id, "py-base", "f5");
        f.cert_sha256 = "00".repeat(32);
        assert!(fetch(&tags, "py-base", &f).await.is_err());

        assert!(held(&tags, Duration::ZERO).is_empty());
        assert_eq!(std::fs::read_dir(&tags).map(|d| d.count()).unwrap_or(0), 0);
    }

    #[tokio::test]
    async fn children_arriving_together_share_one_fetch() {
        let (src, dst) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        image(&src.path().join("tags/py-base"));
        let (addr, id) = source(src.path().join("tags"), &src.path().join("pki")).await;
        let tags = dst.path().join("tags");
        let f = from(&addr, &id, "py-base", "f5");

        let fetches = Arc::new(Fetches::default());
        assert_eq!(fetches.ensure(&tags, "py-base", &f), Status::Pending);
        // A second child while the first fetch runs does not start another.
        assert_eq!(fetches.ensure(&tags, "py-base", &f), Status::Pending);
        assert_eq!(fetches.state.lock().len(), 1);

        for _ in 0..200 {
            if fetches.ensure(&tags, "py-base", &f) == Status::Ready {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the fetch never landed");
    }

    #[tokio::test]
    async fn a_failed_fetch_is_reported_once_and_then_tried_again() {
        let dst = tempfile::tempdir().unwrap();
        let tags = dst.path().join("tags");
        let mut f = from(
            "127.0.0.1:1",
            &super::super::AgentIdentity::load_or_create(&dst.path().join("pki")).unwrap(),
            "t",
            "f5",
        );
        f.address = "127.0.0.1:1".into();
        let fetches = Arc::new(Fetches::default());
        assert_eq!(fetches.ensure(&tags, "t", &f), Status::Pending);
        let mut failed = None;
        for _ in 0..200 {
            match fetches.ensure(&tags, "t", &f) {
                Status::Failed(e) => {
                    failed = Some(e);
                    break;
                }
                Status::Pending => tokio::time::sleep(Duration::from_millis(10)).await,
                Status::Ready => panic!("nothing was listening"),
            }
        }
        assert!(failed.is_some(), "the failure must surface");
        // Reported once; the next ask starts a fresh fetch.
        assert_eq!(fetches.ensure(&tags, "t", &f), Status::Pending);
    }

    #[test]
    fn only_settled_plain_directories_count_as_held() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("py-base")).unwrap();
        std::fs::create_dir(d.path().join(".fetch-py-base-abc")).unwrap();
        std::fs::write(d.path().join("stray-file"), b"x").unwrap();

        assert_eq!(held(d.path(), Duration::ZERO), vec!["py-base".to_string()]);
        // Just written, so not reported yet.
        assert!(held(d.path(), Duration::from_secs(300)).is_empty());
        assert!(held(&d.path().join("missing"), Duration::ZERO).is_empty());
    }

    #[test]
    fn unfinished_fetches_are_cleared_and_tags_are_not() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("keep")).unwrap();
        std::fs::create_dir(d.path().join(".fetch-keep-123")).unwrap();
        clear_partial(d.path());
        assert!(d.path().join("keep").is_dir());
        assert!(!d.path().join(".fetch-keep-123").exists());
    }

    #[test]
    fn an_image_with_anything_but_plain_files_is_refused() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("checkpoint.img"), b"abc").unwrap();
        std::fs::write(d.path().join("pages.img"), b"defg").unwrap();
        let files = list_image(d.path()).unwrap();
        assert_eq!(
            files
                .iter()
                .map(|f| (f.name.as_str(), f.size))
                .collect::<Vec<_>>(),
            vec![("checkpoint.img", 3), ("pages.img", 4)]
        );
        std::fs::create_dir(d.path().join("sub")).unwrap();
        assert!(list_image(d.path()).is_err());
    }
}
