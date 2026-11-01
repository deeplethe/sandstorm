//! Listen mode: the node waits for a master to connect to it.
//!
//! The agent in `mod.rs` dials the master and asks for work. Here the
//! direction is reversed: the master is given this node's address and
//! pairing code, connects, and hands it commands. That puts dispatch where
//! it reads naturally -- the master decides and sends, the node executes and
//! reports -- and a node that is already installed needs nothing more than
//! to be pointed at.
//!
//! The messages are the same ones the poll carries. A `PollRequest` comes
//! back as the answer, `Command`s go in as the request, and both sides run
//! the same functions on them (`run_group` and `report` here, `diff` and
//! friends on the master), so a listening node and a polling one cannot
//! drift apart in behaviour.
//!
//! ## Trust
//!
//! One code, shown once at startup and kept in the pki directory:
//!
//! ```text
//! <SHA-256 of this node's TLS certificate>.<random secret>
//! ```
//!
//! - The master pins this node's certificate by the fingerprint, so nothing
//!   can stand in for the node.
//! - The master presents the secret on every request, so nothing that can
//!   merely reach the port can drive the node.
//!
//! The code is generated once and then kept, so it is the same after a
//! restart and re-adding the node -- to a rebuilt master, say -- uses the
//! same code. That makes it a standing credential: whoever holds it can
//! start, stop and exec into every sandbox here, which is root-equivalent on
//! this machine. Keep it like a root password. It is written to a root-only
//! file rather than the log for that reason. To change it, delete
//! `pairing-secret` (and `agent.pem` + `agent-key.pem` for a new certificate
//! too) and restart.

use anyhow::{bail, Context, Result};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use super::{
    command_id, create_workers, net, next_group, report, run_group, sooner, start_workers, tags,
    wakeups, write_private, Failing, Gateway, NodeConfig, Runtime, Step, Wakeups,
};
use crate::proto::{AgentHello, SyncRequest, SyncResponse};

const CERT: &str = "agent.pem";
const KEY: &str = "agent-key.pem";
const SECRET: &str = "pairing-secret";
const CODE: &str = "pairing-code";

/// This node's listen-mode identity: a self-signed certificate and the
/// secret a master must present.
pub struct Identity {
    pub cert_pem: String,
    pub key_pem: String,
    pub secret: String,
    /// SHA-256 of the certificate, the half of the pairing code a master
    /// pins.
    pub fingerprint: String,
}

impl Identity {
    /// Load from `dir`, creating whatever is missing. Nothing that already
    /// exists is replaced, which is what keeps the code fixed.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let cert_path = dir.join(CERT);
        let key_path = dir.join(KEY);
        let secret_path = dir.join(SECRET);

        if !cert_path.exists() || !key_path.exists() {
            // The name is a placeholder: a master reaches a node by whatever
            // address it was given, and checks the fingerprint, not the name.
            let key = rcgen::KeyPair::generate().context("generating the node key")?;
            let cert = rcgen::CertificateParams::new(vec!["sandstorm-node".to_string()])?
                .self_signed(&key)
                .context("signing the node certificate")?;
            write_private(&key_path.to_path_buf(), &key.serialize_pem())?;
            std::fs::write(&cert_path, cert.pem())
                .with_context(|| format!("writing {}", cert_path.display()))?;
        }
        if !secret_path.exists() {
            write_private(&secret_path.to_path_buf(), &crate::identity::random_token())?;
        }

        let read = |p: &Path| {
            std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))
        };
        let cert_pem = read(&cert_path)?;
        let key_pem = read(&key_path)?;
        let secret = read(&secret_path)?.trim().to_string();
        if secret.is_empty() {
            bail!(
                "{} is empty; delete it and restart to generate a new pairing code",
                secret_path.display()
            );
        }
        let fingerprint = crate::tls::fingerprint_of_pem(&cert_pem)?;
        Ok(Self {
            cert_pem,
            key_pem,
            secret,
            fingerprint,
        })
    }

    /// What an operator gives the master. The same shape as a registration
    /// token, `<pin>.<secret>`, and split by the same function.
    pub fn pairing_code(&self) -> String {
        crate::tls::join_token(&self.fingerprint, &self.secret)
    }
}

struct Agent {
    cfg: NodeConfig,
    secret: String,
    hello: AgentHello,
    /// One master drives a node at a time, and the runtime and failure book
    /// are not built to be driven twice at once, so a sync holds this for
    /// its whole length.
    inner: tokio::sync::Mutex<Inner>,
}

struct Inner {
    rt: Runtime,
    failing: HashMap<String, Failing>,
    gateway: Gateway,
    wake: Arc<Wakeups>,
}

fn authorised(agent: &Agent, headers: &HeaderMap) -> bool {
    use subtle::ConstantTimeEq;
    let got = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    // The length is not a secret -- every code has the same one.
    got.len() == agent.secret.len() && bool::from(got.as_bytes().ct_eq(agent.secret.as_bytes()))
}

type Refusal = (StatusCode, Json<serde_json::Value>);

fn refuse(status: StatusCode, msg: impl std::fmt::Display) -> Refusal {
    (
        status,
        Json(serde_json::json!({ "error": msg.to_string() })),
    )
}

fn unauthorised() -> Refusal {
    refuse(
        StatusCode::UNAUTHORIZED,
        "this node needs the secret half of its pairing code",
    )
}

/// What this node is, for a master deciding whether and where to add it.
async fn hello(
    State(agent): State<Arc<Agent>>,
    headers: HeaderMap,
) -> Result<Json<AgentHello>, Refusal> {
    if !authorised(&agent, &headers) {
        return Err(unauthorised());
    }
    Ok(Json(agent.hello.clone()))
}

/// Apply a batch from the master, then report what is here now.
async fn sync(
    State(agent): State<Arc<Agent>>,
    headers: HeaderMap,
    Json(req): Json<SyncRequest>,
) -> Result<Json<SyncResponse>, Refusal> {
    if !authorised(&agent, &headers) {
        return Err(unauthorised());
    }
    let mut guard = agent.inner.lock().await;
    let Inner {
        rt,
        failing,
        gateway,
        wake,
    } = &mut *guard;

    let now = Instant::now();
    let mut soonest = None;
    let mut results = Vec::new();
    match &req.commands {
        Some(commands) => {
            let asked_about: Vec<String> = commands
                .iter()
                .map(|cmd| command_id(cmd).to_string())
                .collect();
            let mut rest = &commands[..];
            while !rest.is_empty() {
                let (group, after) = next_group(rest);
                rest = after;
                let steps = run_group(&agent.cfg, rt, failing, wake, group, now, create_workers());
                for (cmd, step) in group.iter().zip(steps) {
                    match step {
                        Step::Done => super::expose_created(rt, gateway, cmd).await,
                        Step::Later(wait) => soonest = sooner(soonest, wait),
                        Step::Answered(r) => results.push(r),
                    }
                }
            }
            // Same rule as the poll: a sandbox the master no longer asks
            // about has been quarantined, so stop counting its failures.
            failing.retain(|id, _| asked_about.contains(id));
        }
        None => {
            // A report-only refresh changes nothing, but it still says when
            // the next retry is due, so the master can come back for it.
            for f in failing.values() {
                if f.retry_at > now {
                    soonest = sooner(soonest, f.retry_at - now);
                }
            }
        }
    }

    // After the batch, not before: the master diffs against this, and it
    // has to see what the batch just did.
    let report = report(&agent.cfg, rt, failing, gateway, wake, None)
        .await
        .map_err(|e| refuse(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(Json(SyncResponse {
        report,
        results,
        retry_in_ms: soonest.map(|d| d.as_millis() as u64),
    }))
}

/// Run a node that waits for a master instead of dialing one.
pub async fn serve_listen(cfg: NodeConfig, listen: SocketAddr) -> Result<()> {
    let identity = Identity::load_or_create(&cfg.pki_dir)?;

    // Before any sandbox exists, exactly as in pull mode.
    net::install_rules(&cfg.runtime.net).context("installing the sandbox network rules")?;
    let rt = Runtime::new(cfg.runtime.clone())?;
    tags::clear_partial(&rt.tags_dir());
    let adopted = rt.observe()?;
    if !adopted.is_empty() {
        tracing::info!(count = adopted.len(), "re-adopted running sandboxes");
    }

    let about = AgentHello {
        node_id: cfg.node_id.clone(),
        fingerprint: rt.fingerprint(),
        max_sandboxes: cfg.runtime.max_sandboxes,
        host: Some(rt.host()),
    };

    let code_path = cfg.pki_dir.join(CODE);
    write_private(&code_path, &format!("{}\n", identity.pairing_code()))?;
    // The fingerprint half is public -- it is a hash of a certificate this
    // node presents to anyone who connects. The secret half is not, so it
    // goes to a root-only file rather than into a log anyone in `adm` reads.
    tracing::info!(
        node = %cfg.node_id,
        listen = %listen,
        certificate = %identity.fingerprint,
        code_file = %code_path.display(),
        "waiting for a master; give it this address and the pairing code in code_file"
    );

    let server = crate::tls::plain_server_config(&identity.cert_pem, &identity.key_pem)?;
    // Other nodes in the pool fetch this node's tags here, so a fork of one
    // can run anywhere the image restores. Same port and certificate as the
    // master's routes; a different credential (see `tags`).
    let tag_routes = tags::routes(rt.tags_dir(), identity.secret.clone());
    let wake = wakeups(&cfg, &rt);
    start_workers(&cfg, &rt, &wake);
    let agent = Arc::new(Agent {
        secret: identity.secret.clone(),
        hello: about,
        inner: tokio::sync::Mutex::new(Inner {
            rt,
            failing: HashMap::new(),
            gateway: Gateway::waking(wake.clone()),
            wake,
        }),
        cfg,
    });
    let app = Router::new()
        .route("/v1/agent/hello", get(hello))
        .route("/v1/agent/sync", post(sync))
        .with_state(agent)
        .merge(tag_routes);

    axum_server::bind_rustls(
        listen,
        axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(server)),
    )
    .map(|tls| tls.acceptor(axum_server::accept::NoDelayAcceptor))
    .serve(app.into_make_service())
    .await
    .with_context(|| format!("listening on {listen}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The code must not change across restarts -- the whole point of it is
    /// that an operator can write it down once and use it again.
    #[test]
    fn the_pairing_code_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let first = Identity::load_or_create(dir.path()).unwrap().pairing_code();
        let again = Identity::load_or_create(dir.path()).unwrap().pairing_code();
        assert_eq!(first, again, "a second start must not mint a new code");

        let (pin, secret) = crate::tls::split_token(&first);
        assert_eq!(pin.unwrap().len(), 64, "the pin is a full SHA-256");
        assert_eq!(secret.len(), 32, "128 bits of secret");
    }

    #[test]
    fn deleting_the_secret_is_how_you_change_it() {
        let dir = tempfile::tempdir().unwrap();
        let before = Identity::load_or_create(dir.path()).unwrap();
        std::fs::remove_file(dir.path().join(SECRET)).unwrap();
        let after = Identity::load_or_create(dir.path()).unwrap();
        assert_ne!(before.secret, after.secret);
        // The certificate is untouched, so a master that only needs the new
        // secret does not have to re-pin.
        assert_eq!(before.fingerprint, after.fingerprint);
    }

    #[test]
    fn the_secret_and_key_are_root_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        Identity::load_or_create(dir.path()).unwrap();
        for f in [KEY, SECRET] {
            let mode = std::fs::metadata(dir.path().join(f))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{f} is {mode:o}");
        }
    }
}
