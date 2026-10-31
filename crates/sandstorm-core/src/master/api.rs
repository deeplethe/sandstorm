//! The master's HTTP surface: sandboxes for callers, poll and register for
//! nodes.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, Notify};

use super::metrics;
use crate::auth::{require_token, AuthConfig};
use crate::identity::{random_token, Ca};
use crate::node::BackendKind;
use crate::proto::{
    Command, CommandResult, Egress, FetchFrom, PollRequest, PollResponse, RegisterRequest,
    RegisterResponse,
};
use crate::state::{now_secs, Node, Sandbox, SnapshotTag, Store};
use crate::tier::{IdlePolicy, Tier};

pub struct MasterState {
    /// Private on purpose: `store()` is the only way in, so that no call
    /// site can take the mutex without being timed. See `StoreGuard`.
    store: Mutex<Store>,
    pub ca: Mutex<Ca>,
    pub idle: IdlePolicy,
    /// Range the master allocates sandbox ports from, per node.
    pub port_range: (u16, u16),
    /// Highest network slot a node will hand out, which caps sandboxes per
    /// node at the size of its address plan.
    pub max_net_index: u32,
    /// Refuse to place a sandbox on a node below this much free memory, or
    /// above this memory-pressure average.
    ///
    /// Measured on bm (2 cores, 3.6 GB, 2 GB swap) while filling the node
    /// 20 sandboxes at a time:
    ///
    ///   60 sandboxes   1014 MiB free   PSI avg10  2.5   healthy
    ///   80             673             PSI  4.9         swap engaging
    ///  100             579             PSI 11.8         degrading
    ///  120             408             PSI 15.6         degrading fast
    ///  139             391             PSI 61.8         node stopped
    ///                                                   answering SSH
    ///
    /// The master had no view of any of this: the node reports free memory
    /// and pressure on every poll and the master discarded both, so it kept
    /// accepting creates until the machine was unusable. "It fits" and "it
    /// still works" are different numbers, and admission is what keeps them
    /// apart.
    pub admit: Admission,
    /// Whether the node control channel demands a client certificate.
    ///
    /// True whenever the control plane serves TLS, which is the default.
    /// False only under `--insecure-transport`, where there is no
    /// certificate to demand -- see `NodeIdentity::Untrusted`.
    pub require_node_certs: bool,
    /// One waker per node, so a change to desired state reaches the node's
    /// held-open connection at once instead of waiting for its next poll.
    wake: Mutex<HashMap<String, Arc<Notify>>>,
    /// Exec commands waiting to be handed to a node.
    ///
    /// Tier changes are derived from desired state on every poll, so they
    /// need no queue. Exec runs once and someone is waiting for its output,
    /// so it has to be delivered rather than recomputed.
    exec_queue: Mutex<HashMap<String, VecDeque<Command>>>,
    /// Callers parked on an exec, keyed by exec id.
    pending_exec: Mutex<HashMap<String, oneshot::Sender<CommandResult>>>,
    /// One task per node the master connects to. Kept so that re-adding a
    /// node replaces its driver instead of starting a second one, and so
    /// revoking a node stops the master talking to it.
    drivers: Mutex<HashMap<String, tokio::task::AbortHandle>>,
    /// What this master is doing, for `GET /metrics`.
    pub metrics: metrics::Metrics,
    /// The fleet's recent history, for the console. Set once by `serve`;
    /// empty in tests that build a state without it, where the console's
    /// history routes answer 503.
    pub series: std::sync::OnceLock<Arc<super::series::Series>>,
    /// Bearer token the API requires. `None` leaves it open, which is only
    /// for tests: measured on bm, code inside a sandbox can reach the
    /// master over loopback, so an unauthenticated control plane means a
    /// sandbox can mint node tokens and delete other sandboxes.
    api_token: Option<Arc<String>>,
    /// The address each polling node last reached the master from.
    ///
    /// A node that dials in has no address in the database, and the E2B
    /// front door has to reach its gateway. The source address of its poll
    /// is one the master can route to by construction -- the same reasoning
    /// as `install_script` taking the Host the caller used. In memory only:
    /// every node polls within its hold, so a restart relearns it in
    /// seconds.
    node_addrs: Mutex<HashMap<String, std::net::IpAddr>>,
    /// When an E2B caller last paused a sandbox. See `EXPLICIT_TIER_HOLD`.
    explicit_tier: Mutex<HashMap<String, Instant>>,
    /// Nodes asked to restart onto another backend, the backend, and when.
    /// Nothing is placed on one until it reports that backend or
    /// `SWITCH_HOLD` passes, so a create cannot land on a node about to
    /// leave the format it was placed for.
    switching: Mutex<HashMap<String, (BackendKind, Instant)>>,
}

/// How long a node asked to switch backends is kept out of placement if it
/// never comes back reporting the new one. A restart takes seconds; this is
/// for the node that refused after answering, or never restarted at all.
const SWITCH_HOLD: Duration = Duration::from_secs(120);

/// A held store lock that reports how long it was held when it is dropped.
pub struct StoreGuard<'a> {
    inner: parking_lot::MutexGuard<'a, Store>,
    metrics: &'a metrics::Metrics,
    since: Instant,
}

impl std::ops::Deref for StoreGuard<'_> {
    type Target = Store;
    fn deref(&self) -> &Store {
        &self.inner
    }
}

impl Drop for StoreGuard<'_> {
    fn drop(&mut self) {
        self.metrics.store_held.observe(self.since.elapsed());
    }
}

/// When a node is too loaded to take another sandbox.
#[derive(Debug, Clone, Copy)]
pub struct Admission {
    pub min_free_mib: u64,
    pub max_psi_avg10: f64,
}

impl Default for Admission {
    fn default() -> Self {
        // Below the point where bm began degrading, not at it: PSI is a
        // trailing average, so by the time it reads 10 the node has already
        // been under pressure for a while.
        Self {
            min_free_mib: 512,
            max_psi_avg10: 8.0,
        }
    }
}

impl Admission {
    /// Why this node cannot take another sandbox, if it cannot.
    ///
    /// `load` is how many sandboxes it is already carrying. It is passed in
    /// rather than read here so that the scheduler and the console answer
    /// from the same number: the scheduler needs the count anyway, and two
    /// separate reads could disagree across a concurrent create.
    pub fn refuse(&self, n: &Node, load: u32) -> Option<String> {
        // The cap is checked first because it is the one limit an operator
        // set deliberately; memory and pressure move on their own, so a
        // node that is both full and tight is more usefully reported full.
        //
        // `max_sandboxes` 0 means "as many as memory allows" -- that is what
        // `--max-sandboxes` documents, and it is the default.
        if n.max_sandboxes > 0 && load >= n.max_sandboxes {
            return Some(format!(
                "{load} sandboxes, at its cap of {}",
                n.max_sandboxes
            ));
        }
        // A node that has not reported yet reads as 0 free, which must not
        // be mistaken for an exhausted node.
        if n.free_mib > 0 && n.free_mib < self.min_free_mib {
            return Some(format!(
                "{} MiB free, below the {} MiB floor",
                n.free_mib, self.min_free_mib
            ));
        }
        if n.psi_some_avg10 > self.max_psi_avg10 {
            return Some(format!(
                "memory pressure {:.1} above the {:.1} ceiling",
                n.psi_some_avg10, self.max_psi_avg10
            ));
        }
        None
    }
}

/// Everything the master is configured with, so adding one more knob does
/// not mean another positional argument nobody can read at the call site.
#[derive(Debug, Clone)]
pub struct MasterOptions {
    pub idle: IdlePolicy,
    pub port_range: (u16, u16),
    pub admit: Admission,
    pub api_token: Option<String>,
    pub max_net_index: u32,
    /// See `MasterState::require_node_certs`.
    pub require_node_certs: bool,
}

impl Default for MasterOptions {
    fn default() -> Self {
        Self {
            idle: IdlePolicy::default(),
            port_range: (20000, 32000),
            admit: Admission::default(),
            api_token: None,
            // A /16 in /30s.
            // The top slot of a /16 is the node's own; see `NetConfig::reserved_index`.
            max_net_index: 16382,
            // Tests build the router without a TLS listener, so there is no
            // certificate to demand; the live default comes from
            // MasterConfig and is the opposite.
            require_node_certs: false,
        }
    }
}

impl MasterState {
    /// The CA fingerprint a node should pin, for embedding in a token.
    pub fn ca_pin(&self) -> Option<String> {
        crate::tls::fingerprint_of_pem(&self.ca.lock().cert_pem()).ok()
    }

    pub fn new(store: Store, ca: Ca, opts: MasterOptions) -> Self {
        Self {
            store: Mutex::new(store),
            ca: Mutex::new(ca),
            idle: opts.idle,
            port_range: opts.port_range,
            admit: opts.admit,
            wake: Mutex::new(HashMap::new()),
            exec_queue: Mutex::new(HashMap::new()),
            pending_exec: Mutex::new(HashMap::new()),
            drivers: Mutex::new(HashMap::new()),
            api_token: opts.api_token.map(Arc::new),
            node_addrs: Mutex::new(HashMap::new()),
            explicit_tier: Mutex::new(HashMap::new()),
            switching: Mutex::new(HashMap::new()),
            max_net_index: opts.max_net_index,
            require_node_certs: opts.require_node_certs,
            metrics: metrics::Metrics::default(),
            series: std::sync::OnceLock::new(),
        }
    }

    /// Take the store, timing both the wait for it and the hold.
    ///
    /// The field is private so this is the only way in. Every handler and
    /// the demotion sweep pass through here, which is what makes
    /// `sandstorm_store_lock_wait_seconds` a complete picture of control-plane
    /// contention rather than a sample of whoever remembered to measure.
    ///
    /// One mutex guards one sqlite connection, so this is where the master
    /// serialises; a load test that cannot see this number cannot tell a
    /// slow node from a queue in front of the database.
    pub fn store(&self) -> StoreGuard<'_> {
        let queued = Instant::now();
        let inner = self.store.lock();
        self.metrics.store_wait.observe(queued.elapsed());
        StoreGuard {
            inner,
            metrics: &self.metrics,
            since: Instant::now(),
        }
    }

    /// Record a node's driver, stopping any earlier one for the same node.
    pub(super) fn set_driver(&self, node_id: &str, handle: tokio::task::AbortHandle) {
        if let Some(old) = self.drivers.lock().insert(node_id.to_string(), handle) {
            old.abort();
        }
    }

    pub(super) fn stop_driver(&self, node_id: &str) {
        if let Some(h) = self.drivers.lock().remove(node_id) {
            h.abort();
        }
    }

    /// The token a freshly registered node needs for its later calls.
    fn api_token(&self) -> String {
        self.api_token.as_deref().cloned().unwrap_or_default()
    }

    /// Whether `presented` is the API token. True for anything when the API
    /// is open, which only tests do.
    pub(super) fn api_token_matches(&self, presented: &str) -> bool {
        match self.api_token.as_deref() {
            Some(t) => crate::auth::secrets_match(presented, t),
            None => true,
        }
    }

    /// An E2B caller paused this sandbox: keep it paused against traffic
    /// that was already on its way. See `EXPLICIT_TIER_HOLD`.
    pub(super) fn hold_pause(&self, id: &str) {
        let mut held = self.explicit_tier.lock();
        held.retain(|_, at| at.elapsed() < EXPLICIT_TIER_HOLD);
        held.insert(id.to_string(), Instant::now());
    }

    /// The pause is undone by the caller, so traffic may wake it again.
    pub(super) fn release_pause(&self, id: &str) {
        self.explicit_tier.lock().remove(id);
    }

    /// Where to reach a node's gateway from here: the address of a node the
    /// master connects to, or the one a polling node last came from.
    pub(super) fn node_ip(&self, store: &Store, node_id: &str) -> Option<std::net::IpAddr> {
        if let Ok(Some(a)) = store.agent_node(node_id) {
            let host = a
                .address
                .rsplit_once(':')
                .map_or(a.address.as_str(), |(h, _)| h);
            if let Ok(ip) = host.trim_matches(['[', ']']).parse() {
                return Some(ip);
            }
        }
        self.node_addrs.lock().get(node_id).copied()
    }

    pub(super) fn waker(&self, node_id: &str) -> Arc<Notify> {
        self.wake
            .lock()
            .entry(node_id.to_string())
            .or_default()
            .clone()
    }

    /// Tell a node something it holds has changed. `notify_one` leaves a
    /// permit behind, so a change that lands between the node computing its
    /// work and settling down to wait is not lost -- it just wakes
    /// immediately on the next wait.
    ///
    /// The cost is one spurious early return when a wake arrives while no
    /// request is parked: the next park consumes the stale permit, finds
    /// nothing to do and reopens. It is self-correcting, since that second
    /// park has no permit, and losing a real change would be far worse.
    pub fn wake_node(&self, node_id: &str) {
        self.waker(node_id).notify_one();
    }

    /// The backend this node is being moved to, while that is under way.
    fn switching_to(&self, node_id: &str) -> Option<BackendKind> {
        let mut s = self.switching.lock();
        s.retain(|_, (_, at)| at.elapsed() < SWITCH_HOLD);
        s.get(node_id).map(|(k, _)| *k)
    }

    /// Whether this node said it can start this template on its backend.
    /// A node that has not said is taken to, as before nodes said.
    fn can_start(&self, node_id: &str, template: &str) -> bool {
        self.metrics
            .node_templates
            .lock()
            .get(node_id)
            .is_none_or(|ts| ts.iter().any(|t| t == template))
    }
}

/// Which backend an engine's `--version` names: runsc says
/// `runsc version release-…`, Firecracker `Firecracker v…`.
pub(super) fn backend_of_engine(engine: &str) -> Option<BackendKind> {
    let e = engine.trim().to_ascii_lowercase();
    if e.starts_with("runsc") {
        Some(BackendKind::Runsc)
    } else if e.starts_with("firecracker") {
        Some(BackendKind::Firecracker)
    } else {
        None
    }
}

pub(super) type St = Arc<MasterState>;

/// After this many consecutive failures the master stops issuing commands
/// for a sandbox. Retrying a hopeless command for ever is not resilience:
/// it is a permanent CPU cost on the node and an error nobody looks at.
/// The sandbox shows as failed, and `POST /v1/sandboxes/:id/retry` resumes
/// it once whatever was wrong has been fixed.
pub const MAX_FAILURES: u32 = 5;

/// Port a sandbox listens on inside its own namespace. Fixed rather than
/// allocated: each sandbox has its own network namespace, so two may both
/// use it, and a node-wide port space no longer exists.
pub const DEFAULT_SANDBOX_PORT: u16 = 8080;

/// Most children one fork may make at once. A bound rather than a limit
/// anyone should hit: it stops a typo in `n` from filling a node's address
/// plan before admission control gets a word in.
pub const MAX_FORK_WIDTH: u32 = 256;

/// How long an E2B pause is kept against traffic.
///
/// Traffic reaches the master as a report, after the fact: a node that
/// listens reports every 5 s. A request answered just before a pause
/// therefore arrives just after it, and was read as a wake -- measured on
/// bm, the E2B SDK's `beta_pause` came back `running` because the previous
/// call's report landed 50 ms behind it. Within this window the report
/// still marks the sandbox used; it only does not overrule the pause. The
/// same length as the node's own grace, which defers the demotion itself
/// while recent traffic drains.
///
/// E2B's pause only: `/v1`'s tier requests keep the rule that traffic at
/// the gateway is a wake, which the idle tiers depend on.
const EXPLICIT_TIER_HOLD: Duration = Duration::from_secs(15);

pub fn router(st: St) -> Router {
    // Open: a health probe needs no credential, and registration carries
    // its own single-use token -- a node has nothing else yet.
    // Open because a machine that has not joined yet holds no credential:
    // the registration token is what gates joining, and these three are
    // what the token's one-line command needs before it has one.
    let open = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/node/register", post(register))
        .route("/", get(console_page))
        // The page only: everything it shows comes from the guarded
        // routes below, with the token the operator types into it.
        .route("/console", get(console_page))
        .route("/install.sh", get(install_script))
        .route("/ca.pem", get(ca_pem))
        .route("/sandstormd", get(own_binary))
        .with_state(st.clone());

    let cfg = match st.api_token.as_ref() {
        Some(t) => AuthConfig::with_token(t.as_str()),
        None => AuthConfig::open(),
    };
    // The node control channel. It carries the bearer token like everything
    // else, but a token alone is not enough here: it is one shared secret,
    // so any holder could poll as any node and be handed its sandboxes.
    // These two routes additionally require a client certificate, and the
    // node id is taken from it rather than from the body.
    let node_cfg = match st.api_token.as_ref() {
        Some(t) => AuthConfig::with_token(t.as_str()),
        None => AuthConfig::open(),
    };
    let node_routes = Router::new()
        .route("/v1/node/poll", post(poll))
        .route("/v1/node/traffic", post(node_traffic))
        .route("/v1/node/result", post(command_result))
        .layer(axum::middleware::from_fn({
            let st = st.clone();
            move |req, next| {
                let st = st.clone();
                async move { require_node_cert(st, req, next).await }
            }
        }))
        .layer(axum::middleware::from_fn(move |req, next| {
            let cfg = node_cfg.clone();
            async move { require_token(cfg, req, next).await }
        }))
        .with_state(st.clone());

    let guarded = Router::new()
        .route("/v1/nodes", get(list_nodes).post(add_node))
        .route("/v1/node-tokens", post(create_node_token))
        .route("/v1/node-bootstrap", post(node_bootstrap))
        .route("/v1/node/revoke/:id", post(revoke_node))
        .route("/v1/nodes/:id/backend", post(set_node_backend))
        .route("/v1/sandboxes", get(list_sandboxes).post(create_sandbox))
        .route("/v1/sandboxes/:id", get(get_sandbox).delete(delete_sandbox))
        .route("/v1/sandboxes/:id/touch", post(touch_sandbox))
        .route("/v1/sandboxes/:id/retry", post(retry_sandbox))
        .route("/v1/sandboxes/:id/tier", post(set_tier))
        .route("/v1/sandboxes/:id/egress", post(set_sandbox_egress))
        .route(
            "/v1/egress",
            get(get_default_egress).post(set_default_egress),
        )
        .route("/v1/sandboxes/:id/exec", post(exec_sandbox))
        .route("/v1/sandboxes/:id/branch", post(branch_sandbox))
        .route("/v1/sandboxes/:id/eval", post(eval_sandbox))
        .route(
            "/v1/sandboxes/:id/ping",
            post(ping_sandbox).get(ping_sandbox),
        )
        .route("/v1/snapshots", get(list_snapshots))
        .route("/v1/snapshots/:tag", axum::routing::delete(delete_snapshot))
        .route("/v1/snapshots/:tag/info", get(snapshot_info))
        .route("/v1/templates", get(list_templates).post(put_template))
        .route("/v1/templates/from-image", post(pull_template))
        .route("/v1/templates/pulls", get(list_pulls))
        // Behind the token like everything else: the exposition names every
        // node and its free memory, which is a map of the fleet.
        .route("/metrics", get(metrics_endpoint))
        .route("/v1/console/now", get(console_now))
        .route("/v1/console/series", get(console_series))
        .route("/v1/console/events", get(console_events))
        .layer(axum::middleware::from_fn(move |req, next| {
            let cfg = cfg.clone();
            async move { require_token(cfg, req, next).await }
        }))
        .with_state(st.clone());

    // Outermost, so it sees every request including the ones the auth layer
    // rejects -- a load test that is failing on 401s should be able to tell.
    open.merge(node_routes)
        .merge(guarded)
        .layer(axum::middleware::from_fn(move |req, next| {
            let st = st.clone();
            async move { track_request(st, req, next).await }
        }))
}

/// Time every request and file it under the route it matched.
///
/// `MatchedPath` rather than the URI: it is `/v1/sandboxes/:id`, so a
/// thousand sandbox ids stay one series instead of becoming a thousand.
async fn track_request(
    st: St,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let route = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|m| m.as_str().to_string())
        // A request that matched nothing is a 404. Bucketing them together
        // keeps a scanner from inventing a series per probed path.
        .unwrap_or_else(|| "<unmatched>".to_string());
    let method = req.method().as_str().to_string();
    let started = Instant::now();
    let res = next.run(req).await;
    st.metrics
        .observe_http(&method, &route, res.status().as_u16(), started.elapsed());
    res
}

async fn metrics_endpoint(State(st): State<St>) -> Result<impl IntoResponse, ApiErr> {
    let body = metrics::render_with(&st)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok((
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    ))
}

// ---- console ---------------------------------------------------------------

/// The console: one file, served from this binary at `/` and `/console`.
///
/// Open, like any login page -- it holds no secrets. The API token is typed
/// into it and kept in that tab's sessionStorage, so it never reaches disk
/// through the page and is gone when the tab closes. Every call the page
/// makes goes through the same guarded API as any other client.
///
/// Built into the binary rather than shipped separately so the console is
/// always the version of the master it is talking to, and so a single-box
/// deployment stays a single file. The page is `web/console`, built to one
/// file; `build.rs` puts it, or a page saying it was not built, in OUT_DIR.
async fn console_page() -> impl IntoResponse {
    (
        [("content-type", "text/html; charset=utf-8")],
        include_str!(concat!(env!("OUT_DIR"), "/console.html")),
    )
}

#[derive(Deserialize)]
struct SeriesQuery {
    /// How far back, in seconds. Capped at what is kept.
    #[serde(default = "default_since")]
    since: i64,
    /// Seconds per point. Defaults to whatever gives about 240 points.
    #[serde(default)]
    step: Option<i64>,
}

fn default_since() -> i64 {
    900
}

fn console_history(st: &St) -> Result<Arc<super::series::Series>, ApiErr> {
    st.series.get().cloned().ok_or_else(|| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            "this master is not recording history",
        )
    })
}

#[derive(Serialize)]
struct SeriesView {
    now: i64,
    since: i64,
    step: i64,
    every: u64,
    lines: Vec<super::series::Line>,
}

async fn console_series(
    State(st): State<St>,
    axum::extract::Query(q): axum::extract::Query<SeriesQuery>,
) -> Result<Json<SeriesView>, ApiErr> {
    let series = console_history(&st)?;
    let span = q.since.clamp(60, super::series::KEEP.as_secs() as i64);
    let every = super::series::EVERY.as_secs() as i64;
    let step = q.step.unwrap_or(span / 240).max(every);
    let now = now_secs();
    let since = now - span;
    let lines = tokio::task::spawn_blocking(move || series.lines(since, step))
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(SeriesView {
        now,
        since,
        step,
        every: every as u64,
        lines,
    }))
}

#[derive(Deserialize)]
struct EventsQuery {
    #[serde(default = "default_events_since")]
    since: i64,
    #[serde(default = "default_events_limit")]
    limit: u32,
    #[serde(default)]
    offset: u32,
    /// Comma-separated event kinds; all of them if absent.
    #[serde(default)]
    kind: Option<String>,
    /// Matched against subject and detail, ignoring case.
    #[serde(default)]
    q: Option<String>,
}

fn default_events_since() -> i64 {
    3600
}

fn default_events_limit() -> u32 {
    100
}

/// A page of what the master recorded, newest first, with the number that
/// match in `x-total-count` -- the same shape as `GET /v1/sandboxes`.
async fn console_events(
    State(st): State<St>,
    axum::extract::Query(q): axum::extract::Query<EventsQuery>,
) -> Result<impl IntoResponse, ApiErr> {
    let series = console_history(&st)?;
    let since = now_secs() - q.since.max(0);
    let limit = q.limit.clamp(1, 1000);
    let kinds: Vec<String> = q
        .kind
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(String::from)
        .collect();
    let needle = q.q.unwrap_or_default().trim().to_string();
    let (ev, total) = tokio::task::spawn_blocking(move || {
        series.events_page(since, &kinds, &needle, limit, q.offset)
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(([("x-total-count", total.to_string())], Json(ev)))
}

#[derive(Serialize)]
struct ConsoleNode {
    id: String,
    pool: String,
    /// The pool's own hash; a node in a derived pool `<asked>#<fp8>` landed
    /// there because this differs from the rest of `<asked>`.
    fingerprint: String,
    /// What `fingerprint` is made of, once the node has reported it.
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<crate::proto::HostInfo>,
    connected: bool,
    last_seen_age: i64,
    free_mib: u64,
    psi: f64,
    max_sandboxes: u32,
    tiers: HashMap<String, u32>,
    features: Vec<String>,
    /// Snapshot tags baked on this node. Like its sandboxes, they are in its
    /// backend's format, so they too keep it from switching.
    snapshots: u32,
    /// The templates it can start on its backend, once it has said.
    #[serde(skip_serializing_if = "Option::is_none")]
    templates: Option<Vec<String>>,
    /// `runsc` or `firecracker` while it restarts onto that backend.
    #[serde(skip_serializing_if = "Option::is_none")]
    switching_to: Option<&'static str>,
}

#[derive(Serialize)]
struct ConsoleFailing {
    id: String,
    node: String,
    error: String,
    attempts: u32,
}

#[derive(Serialize)]
struct ConsoleNow {
    now: i64,
    /// Which pool each node was in, and when, so the console can add the
    /// per-node history up per pool.
    pool_history: Vec<crate::state::PoolStretch>,
    uptime_secs: u64,
    idle: [u64; 3],
    tiers: HashMap<String, u32>,
    failed: u32,
    snapshots: u32,
    snapshot_bytes: u64,
    templates: Vec<String>,
    nodes: Vec<ConsoleNode>,
    failing: Vec<ConsoleFailing>,
    counters: HashMap<&'static str, u64>,
    /// The admission thresholds a node is held to, so the console marks
    /// the figures it actually refuses at rather than the defaults.
    floor_mib: u64,
    max_psi: f64,
}

/// Everything the console draws that is a single "now" rather than a line.
async fn console_now(State(st): State<St>) -> Result<Json<ConsoleNow>, ApiErr> {
    let ise = |e: anyhow::Error| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    let (
        nodes,
        by_tier,
        by_node_tier,
        failed,
        (snapshots, snapshot_bytes),
        failing,
        templates,
        mut snaps_by_node,
    ) = {
        let store = st.store();
        (
            store.list_nodes().map_err(ise)?,
            store.count_sandboxes_by_tier().map_err(ise)?,
            store.count_sandboxes_by_node_tier().map_err(ise)?,
            store.count_failed_sandboxes().map_err(ise)?,
            store.snapshot_totals().map_err(ise)?,
            store.failing_sandboxes(12).map_err(ise)?,
            store.list_templates().map_err(ise)?,
            store.count_snapshots_by_node().map_err(ise)?,
        )
    };
    let now = now_secs();
    let features = st.metrics.node_features.lock().clone();
    let node_templates = st.metrics.node_templates.lock().clone();
    let (mut hosts, pool_history) = {
        let store = st.store();
        (
            store.node_hosts().map_err(ise)?,
            store.node_pools().map_err(ise)?,
        )
    };
    let mut tiers_by_node: HashMap<String, HashMap<String, u32>> = HashMap::new();
    for (node, tier, n) in by_node_tier {
        tiers_by_node.entry(node).or_default().insert(tier, n);
    }
    let m = &st.metrics;
    let demoted = m.demotions_total();
    let counters = HashMap::from([
        ("creates_placed", m.creates_placed.load(Relaxed)),
        ("creates_refused", m.creates_refused.load(Relaxed)),
        ("deletes", m.deletes.load(Relaxed)),
        ("woken", m.woken.load(Relaxed)),
        ("evicted", m.evicted.load(Relaxed)),
        ("to_frozen", demoted[1]),
        ("to_snapshot", demoted[2]),
        ("to_detached", demoted[3]),
    ]);
    Ok(Json(ConsoleNow {
        now,
        pool_history,
        floor_mib: st.admit.min_free_mib,
        max_psi: st.admit.max_psi_avg10,
        uptime_secs: m.uptime().as_secs(),
        idle: [
            st.idle.to_frozen.as_secs(),
            st.idle.to_snapshot.as_secs(),
            st.idle.to_detached.as_secs(),
        ],
        tiers: by_tier,
        failed,
        snapshots,
        snapshot_bytes,
        templates,
        nodes: nodes
            .into_iter()
            .map(|n| ConsoleNode {
                last_seen_age: (now - n.last_seen_at).max(0),
                tiers: tiers_by_node.remove(&n.id).unwrap_or_default(),
                features: features.get(&n.id).cloned().unwrap_or_default(),
                snapshots: snaps_by_node.remove(&n.id).unwrap_or(0),
                templates: node_templates.get(&n.id).cloned(),
                switching_to: st.switching_to(&n.id).map(BackendKind::as_str),
                host: hosts.remove(&n.id),
                id: n.id,
                pool: n.pool,
                fingerprint: n.fingerprint,
                connected: n.connected,
                free_mib: n.free_mib,
                psi: n.psi_some_avg10,
                max_sandboxes: n.max_sandboxes,
            })
            .collect(),
        failing: failing
            .into_iter()
            .map(|(id, node, error, attempts)| ConsoleFailing {
                id,
                node,
                error,
                attempts,
            })
            .collect(),
        counters,
    }))
}

/// The header a sandbox's own access token travels in.
///
/// A header, not the body: `ping` is a GET, and a query parameter would put
/// the secret in logs and in `Referer`.
pub const SANDBOX_TOKEN_HEADER: &str = "x-sandstorm-sandbox-token";

/// Check the caller holds *this sandbox's* token (RFC 16.5, layer two).
///
/// The API token is one credential for the whole deployment: holding it
/// means holding every sandbox. This is the second layer -- "who may enter
/// this sandbox" as distinct from "who may be a node" -- and it is why
/// knowing a sandbox id is no longer enough to reach into it.
///
/// Mandatory by design. v0.5's traffic token was opt-in, so every sandbox
/// made before anyone turned it on was open; the RFC removes that mode
/// rather than defaulting it on.
fn require_sandbox_token(st: &St, id: &str, headers: &axum::http::HeaderMap) -> Result<(), ApiErr> {
    let expected = {
        let store = st.store();
        store
            .get_sandbox(id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .map(|s| s.token)
    };
    let Some(expected) = expected else {
        return Err(err(StatusCode::NOT_FOUND, format!("no sandbox {id}")));
    };
    let presented = headers
        .get(SANDBOX_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if crate::auth::secrets_match(presented, &expected) {
        return Ok(());
    }
    // 403 rather than 404: the caller already proved it may talk to this
    // API, and pretending the sandbox does not exist would only make a
    // missing header look like a bug in their code.
    Err(err(
        StatusCode::FORBIDDEN,
        format!(
            "this sandbox requires its own access token in the \
             {SANDBOX_TOKEN_HEADER} header; it is the `token` field of the \
             response that created the sandbox"
        ),
    ))
}

/// The join script, with this master's address filled in.
///
/// `POST /v1/node-bootstrap` hands out a one-liner that fetches this. It
/// used to name `/install.sh` without anything serving it, so the command
/// the panel produced returned 404 -- the endpoint existed and its output
/// could not work.
async fn install_script(headers: axum::http::HeaderMap) -> impl IntoResponse {
    // The master cannot know its own external address -- it may be bound to
    // 0.0.0.0 behind a NAT -- so the URL comes from the Host the caller
    // used, which is by definition one that reaches it.
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("127.0.0.1:8080");
    let body = include_str!("../../../../packaging/install.sh")
        .replace("@MASTER@", &format!("https://{host}"));
    (
        [("content-type", "text/x-shellscript; charset=utf-8")],
        body,
    )
}

/// The CA certificate, so the join script can check it against the
/// fingerprint its token carries. A certificate is a public key; serving it
/// gives away nothing that verifying it does not already require.
async fn ca_pem(State(st): State<St>) -> impl IntoResponse {
    (
        [("content-type", "application/x-pem-file")],
        st.ca.lock().cert_pem(),
    )
}

/// This master's own executable.
///
/// A node runs the same build as its master, which is the simplest way to
/// keep them from drifting -- and it means joining a machine needs no
/// release infrastructure and no internet, which matters because the
/// machines sandstorm is developed on have none.
///
/// The binary is not a secret: sandstorm is open source and the registration
/// token, not obscurity, is what gates joining.
async fn own_binary() -> Result<impl IntoResponse, ApiErr> {
    let bytes = std::fs::read("/proc/self/exe").map_err(|e| {
        err(
            StatusCode::NOT_IMPLEMENTED,
            format!("this master cannot serve its own binary: {e}"),
        )
    })?;
    Ok((
        [
            ("content-type", "application/octet-stream"),
            ("content-disposition", "attachment; filename=\"sandstormd\""),
        ],
        bytes,
    ))
}

/// What the transport established about the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeIdentity {
    /// The connection presented a certificate the master issued to this node.
    Verified(String),
    /// The control plane is running without TLS, so there is nothing to
    /// verify. Kept as a distinct case rather than collapsing into
    /// `Verified(whatever the body claimed)` so that nothing downstream can
    /// mistake an unverified id for a proven one.
    Untrusted,
}

impl NodeIdentity {
    /// Check the id a request claims against the connection it came in on.
    ///
    /// Under TLS a node holding a valid certificate still cannot act as a
    /// different node; without TLS this can only wave the claim through,
    /// which is why `--insecure-transport` logs what it gives up.
    pub fn authorises(&self, claimed: &str) -> bool {
        match self {
            Self::Verified(id) => id == claimed,
            Self::Untrusted => true,
        }
    }
}

/// Resolve the client certificate on this connection to a node.
async fn require_node_cert(
    st: St,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, ApiErr> {
    if !st.require_node_certs {
        req.extensions_mut().insert(NodeIdentity::Untrusted);
        return Ok(next.run(req).await);
    }
    let presented = req
        .extensions()
        .get::<crate::tls::PeerCert>()
        .and_then(|p| p.0.clone());
    let Some(fp) = presented else {
        return Err(err(
            StatusCode::UNAUTHORIZED,
            "this endpoint requires a node client certificate; register with              `sandstorm node-token` to get one",
        ));
    };
    // Resolved into a value first: a lock guard held as the match scrutinee
    // would still be alive across `next.run(req).await` below.
    let owner = {
        let store = st.store();
        store
            .node_by_cert(&fp)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };
    match owner {
        crate::state::CertOwner::Node(id) => {
            req.extensions_mut().insert(NodeIdentity::Verified(id));
            Ok(next.run(req).await)
        }
        crate::state::CertOwner::Revoked(id) => Err(err(
            StatusCode::FORBIDDEN,
            format!("the certificate for node {id} is revoked"),
        )),
        crate::state::CertOwner::Unknown => Err(err(
            StatusCode::UNAUTHORIZED,
            "this client certificate belongs to no registered node",
        )),
    }
}

/// Revoke a node's certificate. The node keeps its files; the master stops
/// accepting them, which is enough because the node is the side that dials.
async fn revoke_node(
    State(st): State<St>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let gone = st
        .store()
        .revoke_node(&id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !gone {
        return Err(err(StatusCode::NOT_FOUND, format!("no node {id}")));
    }
    // Wake its parked poll so the refusal lands now rather than when the
    // hold expires -- and if the master is the one connecting, stop.
    st.wake_node(&id);
    st.stop_driver(&id);
    tracing::warn!(node = %id, "revoked");
    Ok(Json(serde_json::json!({ "revoked": id })))
}

#[derive(Deserialize)]
struct SetBackendReq {
    /// `gvisor` (or `runsc`) or `firecracker`.
    backend: String,
}

#[derive(Serialize)]
struct BackendSwitchView {
    node: String,
    from: Option<&'static str>,
    to: &'static str,
    /// False when the node was on it already and nothing happened.
    restarting: bool,
    /// What the node said.
    message: String,
}

/// Move a node to another backend.
///
/// A backend switch is a restart onto another engine (`node/switch.rs`),
/// and a sandbox or snapshot on the node is in the old engine's format: it
/// could never be restored afterwards. So it is refused while the master has
/// any on record for the node, and the node refuses again if its disk holds
/// any. The node's fingerprint names its engine, so once it is back it is in
/// another pool -- the console says so before anyone confirms.
///
/// Nothing is placed on the node from the moment this is accepted until it
/// reports the new backend (or `SWITCH_HOLD` passes).
async fn set_node_backend(
    State(st): State<St>,
    Path(id): Path<String>,
    Json(req): Json<SetBackendReq>,
) -> Result<Json<BackendSwitchView>, ApiErr> {
    let want: BackendKind = req
        .backend
        .parse()
        .map_err(|e: anyhow::Error| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let switchable = st
        .metrics
        .node_features
        .lock()
        .get(&id)
        .is_some_and(|f| f.iter().any(|x| x == "backend-switch"));

    // Out of placement before the count, and the count under the store lock
    // a create holds while it places: a create either finished first and is
    // counted, or comes after and passes this node by.
    {
        let mut s = st.switching.lock();
        s.retain(|_, (_, at)| at.elapsed() < SWITCH_HOLD);
        if let Some((k, _)) = s.get(&id) {
            return Err(err(
                StatusCode::CONFLICT,
                format!("{id} is already switching to {k}; wait for it to come back"),
            ));
        }
        s.insert(id.clone(), (want, Instant::now()));
    }
    let release = |e: ApiErr| {
        st.switching.lock().remove(&id);
        e
    };
    let ise = |e: anyhow::Error| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    let from = {
        let store = st.store();
        let Some(node) = store
            .list_nodes()
            .map_err(ise)
            .map_err(release)?
            .into_iter()
            .find(|n| n.id == id)
        else {
            return Err(release(err(StatusCode::NOT_FOUND, format!("no node {id}"))));
        };
        if !node.connected {
            return Err(release(err(
                StatusCode::CONFLICT,
                format!(
                    "{id} is not answering; a switch is a command to the node, so it has to be up"
                ),
            )));
        }
        let sandboxes = store
            .count_sandboxes_on_node(&id)
            .map_err(ise)
            .map_err(release)?;
        let snapshots = store
            .count_snapshots_by_node()
            .map_err(ise)
            .map_err(release)?
            .remove(&id)
            .unwrap_or(0);
        if sandboxes > 0 || snapshots > 0 {
            return Err(release(err(
                StatusCode::CONFLICT,
                format!(
                    "{id} holds {sandboxes} sandbox(es) and {snapshots} snapshot(s). Each is in \
                     its current backend's format and could never be restored on another, \
                     so delete them first; a backend can only change on an empty node"
                ),
            )));
        }
        store
            .node_hosts()
            .map_err(ise)
            .map_err(release)?
            .remove(&id)
            .and_then(|h| backend_of_engine(&h.engine))
    };
    if from == Some(want) {
        st.switching.lock().remove(&id);
        return Ok(Json(BackendSwitchView {
            node: id.clone(),
            from: from.map(BackendKind::as_str),
            to: want.as_str(),
            restarting: false,
            message: format!("already on {want}; nothing to do"),
        }));
    }
    if !switchable {
        return Err(release(err(
            StatusCode::CONFLICT,
            format!(
                "{id} was not installed with a backend file, so its backend is fixed by \
                 its command line. Reinstall it with this master's installer (the console's \
                 \"Add a node\"), or change --backend in its unit by hand"
            ),
        )));
    }

    let result_id = random_token();
    let (tx, rx) = oneshot::channel();
    st.pending_exec.lock().insert(result_id.clone(), tx);
    st.exec_queue
        .lock()
        .entry(id.clone())
        .or_default()
        .push_back(Command::SetBackend {
            backend: want.as_str().into(),
            result_id: result_id.clone(),
        });
    st.wake_node(&id);
    match tokio::time::timeout(Duration::from_secs(60), rx).await {
        Ok(Ok(r)) if r.exit_code == 0 => {
            tracing::warn!(node = %id, ?from, to = %want, "backend switch accepted; the node is restarting");
            Ok(Json(BackendSwitchView {
                node: id.clone(),
                from: from.map(BackendKind::as_str),
                to: want.as_str(),
                restarting: true,
                message: r.stdout,
            }))
        }
        Ok(Ok(r)) => Err(release(err(
            StatusCode::CONFLICT,
            format!("{id} refused: {}", r.stderr.trim()),
        ))),
        _ => {
            st.pending_exec.lock().remove(&result_id);
            // Left out of placement: it may have switched and lost the
            // answer. SWITCH_HOLD lets it back in if it did not.
            Err(err(
                StatusCode::GATEWAY_TIMEOUT,
                format!("{id} did not answer the switch within 60s"),
            ))
        }
    }
}

pub(super) fn err(
    code: StatusCode,
    msg: impl Into<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    (code, Json(serde_json::json!({ "error": msg.into() })))
}
pub(super) type ApiErr = (StatusCode, Json<serde_json::Value>);

// ---- nodes --------------------------------------------------------------

#[derive(Serialize)]
struct NodeView {
    id: String,
    pool: String,
    fingerprint: String,
    max_sandboxes: u32,
    connected: bool,
    last_seen_at: i64,
    free_mib: u64,
    psi_some_avg10: f64,
    /// Present when this node would currently refuse a new sandbox.
    #[serde(skip_serializing_if = "Option::is_none")]
    not_admitting: Option<String>,
}

async fn list_nodes(State(st): State<St>) -> Result<Json<Vec<NodeView>>, ApiErr> {
    // Nodes and their loads are read under one lock, so the console cannot
    // show a node refused against a count taken after someone else's delete.
    let nodes = {
        let store = st.store();
        let nodes = store
            .list_nodes()
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let mut out = Vec::with_capacity(nodes.len());
        for n in nodes {
            let load = store
                .count_sandboxes_on_node(&n.id)
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            out.push((n, load));
        }
        out
    };
    Ok(Json(
        nodes
            .into_iter()
            .map(|(n, load)| NodeView {
                not_admitting: st.admit.refuse(&n, load),
                id: n.id,
                pool: n.pool,
                fingerprint: n.fingerprint,
                max_sandboxes: n.max_sandboxes,
                connected: n.connected,
                last_seen_at: n.last_seen_at,
                free_mib: n.free_mib,
                psi_some_avg10: n.psi_some_avg10,
            })
            .collect(),
    ))
}

// ---- adding a node by address ----------------------------------------------

/// Port a listening node is reached on when the address does not say.
///
/// 80 because that is what the first fleet had open: its security group
/// allowed 22 and 80 inbound and dropped everything else, and nothing was
/// using 80 on the nodes. Any port works; this is only the default.
const AGENT_DEFAULT_PORT: u16 = 80;

#[derive(Deserialize)]
struct AddNode {
    /// `host` or `host:port` the master can reach. IPv6 as `[addr]:port`.
    address: String,
    /// What the node wrote to `pki/node/pairing-code` on first start.
    pairing_code: String,
    /// Defaults to the id the node reports, its `--node-id` or hostname.
    #[serde(default)]
    node_id: Option<String>,
    #[serde(default = "default_pool")]
    pool: String,
}

#[derive(Serialize)]
struct AddedNode {
    id: String,
    pool: String,
    address: String,
    fingerprint: String,
    max_sandboxes: u32,
}

fn normalise_address(raw: &str) -> Result<String, ApiErr> {
    let a = raw.trim();
    let a = a
        .strip_prefix("https://")
        .unwrap_or(a)
        .trim_end_matches('/');
    let bad = |why: &str| err(StatusCode::BAD_REQUEST, format!("address {raw:?}: {why}"));
    if a.is_empty() || a.contains('/') || a.chars().any(char::is_whitespace) {
        return Err(bad("expected host or host:port"));
    }
    if a.starts_with('[') {
        return Ok(if a.contains("]:") {
            a.to_string()
        } else {
            format!("{a}:{AGENT_DEFAULT_PORT}")
        });
    }
    match a.matches(':').count() {
        0 => Ok(format!("{a}:{AGENT_DEFAULT_PORT}")),
        1 => Ok(a.to_string()),
        _ => Err(bad("write an IPv6 address as [addr] or [addr]:port")),
    }
}

/// Take over a node that is listening.
///
/// The node is already installed and running with `--listen`; this is the
/// whole of joining it. The master connects, checks the certificate against
/// the pairing code, proves it holds the secret, learns what the node is,
/// and from then on drives it -- no token for the node to fetch, no URL for
/// it to be told, nothing to run on the node.
async fn add_node(
    State(st): State<St>,
    Json(req): Json<AddNode>,
) -> Result<(StatusCode, Json<AddedNode>), ApiErr> {
    let (pin, secret) = crate::tls::split_token(req.pairing_code.trim());
    let Some(pin) = pin.filter(|p| p.len() == 64 && p.bytes().all(|b| b.is_ascii_hexdigit()))
    else {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "pairing_code should be <64 hex>.<secret>, as the node writes it to \
             pki/node/pairing-code",
        ));
    };
    if secret.is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "pairing_code has no secret part",
        ));
    }
    let pin = pin.to_ascii_lowercase();
    let address = normalise_address(&req.address)?;

    let about = super::drive::hello(&address, &pin, secret)
        .await
        .map_err(|e| {
            err(
                StatusCode::BAD_GATEWAY,
                format!("could not take over {address}: {e:#}"),
            )
        })?;
    let node_id = req.node_id.unwrap_or(about.node_id);
    if !crate::tag::is_safe(&node_id) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("node id {node_id:?} is not a plain name"),
        ));
    }

    // Two drivers for one node -- this one, and the node itself still
    // polling under the same id -- would each be handed its sandboxes.
    {
        let store = st.store();
        let polling_now = store
            .list_nodes()
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .into_iter()
            .any(|n| n.id == node_id && n.connected && now_secs() - n.last_seen_at < 60);
        let already_ours = store
            .agent_nodes()
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .iter()
            .any(|a| a.id == node_id);
        if polling_now && !already_ours {
            return Err(err(
                StatusCode::CONFLICT,
                format!(
                    "node {node_id} is connected and polling right now; stop its \
                     pull-mode service first, or add this one under another node_id"
                ),
            ));
        }
    }

    let pool = pool_for(&st, req.pool, &about.fingerprint, &node_id)?;
    {
        let store = st.store();
        store
            .upsert_node(&Node {
                id: node_id.clone(),
                pool: pool.clone(),
                fingerprint: about.fingerprint.clone(),
                max_sandboxes: about.max_sandboxes,
                last_seen_at: now_secs(),
                // Connected once the driver's first sync lands, not before.
                connected: false,
                free_mib: 0,
                psi_some_avg10: 0.0,
            })
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        store
            .set_node_agent(&node_id, &address, &pin, secret)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        if let Some(h) = &about.host {
            record_host(&st, &store, &node_id, h)?;
        }
    }
    super::drive::start(
        st.clone(),
        crate::state::AgentNode {
            id: node_id.clone(),
            address: address.clone(),
            cert_sha256: pin,
            secret: secret.to_string(),
        },
    );
    tracing::info!(node = %node_id, %address, pool = %pool, "node added; the master drives it");
    Ok((
        StatusCode::CREATED,
        Json(AddedNode {
            id: node_id,
            pool,
            address,
            fingerprint: about.fingerprint,
            max_sandboxes: about.max_sandboxes,
        }),
    ))
}

#[derive(Deserialize)]
struct NewNodeToken {
    #[serde(default = "default_pool")]
    pool: String,
    /// Short on purpose. A registration token is a bootstrap credential;
    /// it should not outlive the operator's terminal session.
    #[serde(default = "default_ttl")]
    ttl_secs: i64,
}
fn default_pool() -> String {
    "default".into()
}
fn default_ttl() -> i64 {
    900
}

#[derive(Serialize)]
struct NodeTokenView {
    token: String,
    pool: String,
    expires_at: i64,
}

async fn create_node_token(
    State(st): State<St>,
    Json(req): Json<NewNodeToken>,
) -> Result<Json<NodeTokenView>, ApiErr> {
    let secret = random_token();
    st.store()
        .create_node_token(&secret, &req.pool, req.ttl_secs)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    // Handed out with the CA fingerprint so the node can verify the master
    // on the one request that carries this secret.
    let token = st
        .ca_pin()
        .map(|pin| crate::tls::join_token(&pin, &secret))
        .unwrap_or(secret);
    Ok(Json(NodeTokenView {
        token,
        pool: req.pool,
        expires_at: now_secs() + req.ttl_secs,
    }))
}

#[derive(Deserialize)]
struct BootstrapReq {
    #[serde(default = "default_pool")]
    pool: String,
    #[serde(default = "default_ttl")]
    ttl_secs: i64,
    /// How the new machine should reach this master. The master cannot work
    /// this out for itself: the address a node must dial is whatever the
    /// operator's network puts in front of us, not the socket we bound.
    master_url: String,
    /// Which backend the new node runs: `auto` (Firecracker where the
    /// machine has /dev/kvm, gVisor elsewhere), `gvisor` or `firecracker`.
    /// Absent leaves it to the installer, which picks gVisor.
    #[serde(default)]
    backend: Option<String>,
    /// For Firecracker: the guest kernel, if not `<work dir>/vmlinux`.
    #[serde(default)]
    fc_kernel: Option<String>,
    /// The node's work dir, if not /var/lib/sandstorm. Firecracker needs it
    /// on XFS (reflink=1) or btrfs.
    #[serde(default)]
    state_dir: Option<String>,
}

/// The installer variables a bootstrap request sets, checked: they go into a
/// shell command someone runs as root, so a path is held to characters that
/// need no quoting.
fn install_env(req: &BootstrapReq) -> Result<String, ApiErr> {
    let mut env = String::new();
    if let Some(b) = req.backend.as_deref().filter(|b| !b.is_empty()) {
        if !["auto", "gvisor", "runsc", "firecracker"].contains(&b) {
            return Err(err(
                StatusCode::BAD_REQUEST,
                format!("backend must be auto, gvisor or firecracker, not {b:?}"),
            ));
        }
        env.push_str(&format!(" SANDSTORM_BACKEND={b}"));
    }
    for (name, var, v) in [
        ("fc_kernel", "SANDSTORM_FC_KERNEL", &req.fc_kernel),
        ("state_dir", "SANDSTORM_STATE_DIR", &req.state_dir),
    ] {
        let Some(v) = v.as_deref().filter(|v| !v.is_empty()) else {
            continue;
        };
        let plain = v.starts_with('/')
            && !v.contains("..")
            && v.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"/._-".contains(&c));
        if !plain {
            return Err(err(
                StatusCode::BAD_REQUEST,
                format!(
                    "{name} must be an absolute path of letters, digits and . _ - / only, \
                     not {v:?}"
                ),
            ));
        }
        env.push_str(&format!(" {var}={v}"));
    }
    Ok(env)
}

#[derive(Serialize)]
struct BootstrapView {
    token: String,
    pool: String,
    expires_at: i64,
    /// What the console shows for "add a node". It is a command, not a
    /// machine: the master cannot reach a host that has never dialled in,
    /// so one line has to be run over there. Same shape as k3s, Tailscale
    /// or an Actions runner, and for the same reason.
    install_command: String,
    /// The CA's SHA-256, which is also the first half of `token`.
    ///
    /// Shown separately so an operator can compare it out of band, or pass
    /// the CA to the script with `SANDSTORM_CA=` and skip the unverified first
    /// fetch entirely.
    #[serde(skip_serializing_if = "Option::is_none")]
    ca_fingerprint: Option<String>,
    /// Said in the response rather than left to be discovered: the first
    /// fetch of the script cannot be verified, because the fingerprint that
    /// would verify it is inside the script's own argument. Everything the
    /// script does afterwards is checked against that fingerprint.
    first_fetch_is_unverified: bool,
}

/// Mint a token and render the one line that joins a new machine.
async fn node_bootstrap(
    State(st): State<St>,
    Json(req): Json<BootstrapReq>,
) -> Result<Json<BootstrapView>, ApiErr> {
    let env = install_env(&req)?;
    let secret = random_token();
    st.store()
        .create_node_token(&secret, &req.pool, req.ttl_secs)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let token = st
        .ca_pin()
        .map(|pin| crate::tls::join_token(&pin, &secret))
        .unwrap_or(secret);
    let master = req.master_url.trim_end_matches('/');
    let pin = st.ca_pin();
    // `-k` on this one hop only. The master's certificate is signed by its
    // own CA, so plain curl refuses it, and the fingerprint that would
    // verify it is carried in the token the command is passing. The script
    // fetches the CA, checks it against that fingerprint, and does
    // everything after with --cacert. An operator who wants the first hop
    // verified too copies pki/ca.pem over and sets SANDSTORM_CA.
    Ok(Json(BootstrapView {
        install_command: format!(
            "curl -sSfL -k {master}/install.sh | sudo SANDSTORM_MASTER_URL={master} \\\n    SANDSTORM_NODE_TOKEN={token}{env} sh"
        ),
        token,
        pool: req.pool,
        expires_at: now_secs() + req.ttl_secs,
        ca_fingerprint: pin,
        first_fetch_is_unverified: true,
    }))
}

/// Which pool a node with this host fingerprint actually joins.
///
/// A pool is a migration domain, not a label: every node in it must be able
/// to restore every other's checkpoints, and runsc refuses across even a
/// one-week release difference. So a machine whose fingerprint does not
/// match the pool's does not join it -- it gets a pool of its own, derived
/// from the fingerprint. The alternative is a pool that mixes two versions
/// and only says so when a restore fails, which is the failure mode RFC
/// section 15 set out to avoid.
///
/// The same rule whether a node registers or is added by address.
pub(super) fn pool_for(
    st: &St,
    asked: String,
    fingerprint: &str,
    node_id: &str,
) -> Result<String, ApiErr> {
    let store = st.store();
    let seen = store
        .pool_fingerprints(&asked)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mismatched: Vec<&String> = seen.iter().filter(|f| *f != fingerprint).collect();
    if mismatched.is_empty() {
        return Ok(asked);
    }
    let derived = format!("{asked}#{}", &fingerprint[..8.min(fingerprint.len())]);
    tracing::warn!(
        node = %node_id,
        asked = %asked,
        joined = %derived,
        fingerprint = %fingerprint,
        others = ?mismatched,
        "fingerprint does not match this pool; the node joins a derived pool \
         instead so its snapshots are never scheduled onto a host that cannot \
         restore them"
    );
    Ok(derived)
}

/// Trade a single-use registration token for a client certificate.
/// This is the one endpoint a node reaches without a certificate, so it is
/// also the only place the token path can be abused -- hence single use,
/// short TTL, and the node id fixed into the certificate's CN.
async fn register(
    State(st): State<St>,
    Json(req): Json<RegisterRequest>,
) -> Result<Json<RegisterResponse>, ApiErr> {
    let pool = {
        let store = st.store();
        store
            // Only the secret half is stored; the pin is public.
            .redeem_node_token(crate::tls::split_token(&req.token).1, &req.node_id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };
    let Some(pool) = pool else {
        return Err(err(
            StatusCode::UNAUTHORIZED,
            "registration token is unknown, expired, or already used",
        ));
    };

    let pool = pool_for(&st, pool, &req.fingerprint, &req.node_id)?;

    let (cert, key, ca_pem) = {
        let ca = st.ca.lock();
        let paths = ca
            .issue_node(&req.node_id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let cert = std::fs::read_to_string(&paths.cert)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let key = std::fs::read_to_string(&paths.key)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        (cert, key, ca.cert_pem())
    };

    // Tie this certificate to this node before the node can use it, so the
    // very next request already resolves through it.
    let cert_fp = crate::tls::fingerprint_of_pem(&cert)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    st.store()
        .upsert_node(&Node {
            id: req.node_id.clone(),
            pool: pool.clone(),
            fingerprint: req.fingerprint,
            max_sandboxes: req.max_sandboxes,
            last_seen_at: now_secs(),
            connected: true,
            free_mib: 0,
            psi_some_avg10: 0.0,
        })
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    st.store()
        .set_node_cert(&req.node_id, &cert_fp)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if let Some(h) = &req.host {
        record_host(&st, &st.store(), &req.node_id, h)?;
    }

    tracing::info!(node = %req.node_id, pool = %pool, cert = %cert_fp, "node registered");
    Ok(Json(RegisterResponse {
        node_id: req.node_id,
        pool,
        client_cert: cert,
        client_key: key,
        ca_cert: ca_pem,
        // The node's later calls are on the guarded routes, so it is handed
        // the token here -- the one moment it is already authenticated, by
        // the single-use registration token it just spent.
        api_token: st.api_token(),
    }))
}

/// A node reports what it holds and collects work. Desired state is compared
/// against the node's observation, never against a cached view.
async fn poll(
    State(st): State<St>,
    Extension(who): Extension<NodeIdentity>,
    peer: Option<axum::extract::ConnectInfo<std::net::SocketAddr>>,
    Json(req): Json<PollRequest>,
) -> Result<Json<PollResponse>, ApiErr> {
    // The certificate decides which node this is, not the body. Otherwise a
    // node holding any valid certificate could poll as another node and be
    // handed its sandboxes -- and its exec output.
    if !who.authorises(&req.node_id) {
        tracing::warn!(
            claimed = %req.node_id,
            identity = ?who,
            "refused a poll claiming a node id the certificate does not match"
        );
        return Err(err(
            StatusCode::FORBIDDEN,
            "this certificate is not registered for that node id",
        ));
    }
    if let Some(axum::extract::ConnectInfo(addr)) = peer {
        st.node_addrs
            .lock()
            .insert(req.node_id.clone(), addr.ip().to_canonical());
    }
    // Held open, not polled. The node dials in and this request stays
    // parked until there is work for it, so dispatch costs one round trip
    // rather than up to a poll interval -- the operator-visible behaviour
    // of a machine that accepts scheduling the moment its service is up,
    // without the node listening on anything.
    record_failures(&st, &req)?;
    apply_traffic(&st, &req)?;
    apply_evictions(&st, &req)?;

    let mut commands = diff(&st, &req)?;
    leave_out_deferred(&mut commands, &req, Duration::ZERO);
    commands.extend(take_execs(&st, &req.node_id));
    if !commands.is_empty() {
        return Ok(Json(PollResponse { commands }));
    }

    let waker = st.waker(&req.node_id);
    let hold = Duration::from_millis(req.hold_ms.unwrap_or(25_000).min(55_000));
    let parked = Instant::now();
    if !hold.is_zero() {
        // A timeout as well as a wake, so the connection is refreshed
        // periodically and a node that has silently gone away is noticed.
        tokio::time::timeout(hold, waker.notified()).await.ok();
    }
    let mut commands = diff(&st, &req)?;
    leave_out_deferred(&mut commands, &req, parked.elapsed());
    commands.extend(take_execs(&st, &req.node_id));
    Ok(Json(PollResponse { commands }))
}

/// Take out what the node has said it is putting off, `waited` after it said
/// so. `diff` issues a command for everything that has not converged, and an
/// answer with commands in it goes back at once, so without this a node that
/// skips them all polls again straight away until the retry is due. `drive`
/// has the same problem and `same_batch` for it.
fn leave_out_deferred(commands: &mut Vec<Command>, req: &PollRequest, waited: Duration) {
    if req.deferred.is_empty() {
        return;
    }
    let deferred: HashMap<&str, u64> = req
        .deferred
        .iter()
        .map(|d| (d.id.as_str(), d.retry_in_ms))
        .collect();
    commands.retain(|cmd| {
        deferred
            .get(crate::node::command_id(cmd))
            .is_none_or(|ms| Duration::from_millis(*ms) <= waited)
    });
}

/// A report out of turn, sent by a polling node the moment its gateway
/// woke a sandbox.
///
/// The node's regular report rides on its next poll, and the one it has
/// parked here can sit for the whole hold -- 25 s by default. Until then the
/// master shows the old tier, and an explicit request for that same tier is
/// a no-op because desired state already says so (measured on bm: a freeze
/// sent 16 s after a wake did nothing). This closes that window to a round
/// trip. It returns no commands, so it cannot race the parked poll for them.
async fn node_traffic(
    State(st): State<St>,
    Extension(who): Extension<NodeIdentity>,
    Json(req): Json<PollRequest>,
) -> Result<StatusCode, ApiErr> {
    if !who.authorises(&req.node_id) {
        return Err(err(
            StatusCode::FORBIDDEN,
            "this certificate is not registered for that node id",
        ));
    }
    apply_traffic(&st, &req)?;
    apply_evictions(&st, &req)?;
    // The parked poll diffs against the report it arrived with, from before
    // the wake. Wake it so the node comes back with a current one.
    st.wake_node(&req.node_id);
    Ok(StatusCode::NO_CONTENT)
}

pub(super) fn take_execs(st: &St, node_id: &str) -> Vec<Command> {
    let mut q = st.exec_queue.lock();
    match q.get_mut(node_id) {
        Some(d) => d.drain(..).collect(),
        None => Vec::new(),
    }
}

#[derive(Deserialize)]
pub(super) struct SetTierReq {
    pub(super) tier: Tier,
}

/// Ask for a tier directly, rather than waiting for the idle policy.
///
/// Demotion still goes one step at a time -- the node has no direct
/// hot -> detached transition, because each leg is cheap and a jump would
/// stall it on a tar. So this walks the intermediate tiers itself.
pub(super) async fn set_tier(
    State(st): State<St>,
    Path(id): Path<String>,
    Json(req): Json<SetTierReq>,
) -> Result<Json<SandboxView>, ApiErr> {
    let (node_id, view) = {
        let store = st.store();
        let Some(sb) = store
            .get_sandbox(&id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        else {
            return Err(err(StatusCode::NOT_FOUND, "no such sandbox"));
        };
        store
            .set_tier(&id, req.tier)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        if req.tier == Tier::Hot {
            store
                .touch(&id)
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        }
        let fresh = store
            .get_sandbox(&id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .ok_or_else(|| err(StatusCode::NOT_FOUND, "no such sandbox"))?;
        (sb.node_id, fresh)
    };
    st.wake_node(&node_id);
    Ok(Json(view.into()))
}

/// Clear a sandbox's failure so the node is asked to try again.
async fn retry_sandbox(
    State(st): State<St>,
    Path(id): Path<String>,
) -> Result<Json<SandboxView>, ApiErr> {
    let (node_id, view) = {
        let store = st.store();
        let Some(sb) = store
            .get_sandbox(&id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        else {
            return Err(err(StatusCode::NOT_FOUND, "no such sandbox"));
        };
        store
            .clear_sandbox_error(&id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let fresh = store
            .get_sandbox(&id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .ok_or_else(|| err(StatusCode::NOT_FOUND, "no such sandbox"))?;
        (sb.node_id, fresh)
    };
    st.wake_node(&node_id);
    Ok(Json(view.into()))
}

#[derive(Deserialize)]
struct ExecReq {
    args: Vec<String>,
    #[serde(default = "default_exec_timeout")]
    timeout_secs: u64,
}
fn default_exec_timeout() -> u64 {
    30
}

#[derive(Serialize)]
struct ExecView {
    stdout: String,
    stderr: String,
    exit_code: i64,
}

/// Run a command in a sandbox and wait for its output.
///
/// Same request and response shape as the v0.5 controller, so the SDKs do
/// not have to change for this call.
async fn exec_sandbox(
    State(st): State<St>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    Json(req): Json<ExecReq>,
) -> Result<Json<ExecView>, ApiErr> {
    require_sandbox_token(&st, &id, &headers)?;
    if req.args.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "args must not be empty"));
    }
    let r = run_exec(&st, &id, req.args, req.timeout_secs).await?;
    Ok(Json(ExecView {
        stdout: r.stdout,
        stderr: r.stderr,
        exit_code: r.exit_code,
    }))
}

/// Queue an exec for the sandbox's node and wait for the result. Shared by
/// `exec` and `eval`, which differ only in how they build the command and
/// shape the answer.
async fn run_exec(
    st: &St,
    id: &str,
    args: Vec<String>,
    timeout_secs: u64,
) -> Result<CommandResult, ApiErr> {
    let (node_id, tier) = {
        let store = st.store();
        let Some(sb) = store
            .get_sandbox(id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        else {
            return Err(err(StatusCode::NOT_FOUND, "no such sandbox"));
        };
        (sb.node_id, sb.tier)
    };
    // A command can only run in a sandbox that is running. Promoting first
    // would hide a multi-second wait inside what looks like a quick call,
    // so say so instead and let the caller decide.
    if tier != Tier::Hot {
        return Err(err(
            StatusCode::CONFLICT,
            format!("sandbox is {tier}; POST /v1/sandboxes/{id}/touch to bring it hot first"),
        ));
    }

    let exec_id = random_token();
    let (tx, rx) = oneshot::channel();
    {
        st.pending_exec.lock().insert(exec_id.clone(), tx);
        st.exec_queue
            .lock()
            .entry(node_id.clone())
            .or_default()
            .push_back(Command::Exec {
                exec_id: exec_id.clone(),
                id: id.to_string(),
                args,
                timeout_secs,
            });
    }
    st.wake_node(&node_id);

    // The node's own timeout plus room for a round trip and a loaded node.
    let budget = Duration::from_secs(timeout_secs.saturating_add(20));
    match tokio::time::timeout(budget, rx).await {
        Ok(Ok(r)) => Ok(r),
        // Sender dropped, or nothing came back in time. Either way the
        // caller gets an answer rather than hanging.
        _ => {
            st.pending_exec.lock().remove(&exec_id);
            Err(err(
                StatusCode::GATEWAY_TIMEOUT,
                format!(
                    "node {node_id} did not return a result within {}s",
                    budget.as_secs()
                ),
            ))
        }
    }
}

/// The node reporting the outcome of a command someone is waiting on.
/// Hand an exec or branch result to whoever is waiting on it. False when
/// nobody is: the caller timed out, or the id was never issued.
pub(super) fn deliver_result(st: &St, r: CommandResult) -> bool {
    match st.pending_exec.lock().remove(&r.result_id) {
        Some(tx) => {
            // The caller may already have timed out and gone.
            let _ = tx.send(r);
            true
        }
        None => false,
    }
}

async fn command_result(
    State(st): State<St>,
    Json(r): Json<CommandResult>,
) -> Result<StatusCode, ApiErr> {
    if deliver_result(&st, r) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(err(
            StatusCode::NOT_FOUND,
            "no caller is waiting on that result",
        ))
    }
}

#[derive(Deserialize)]
struct EvalReq {
    code: String,
    #[serde(default = "default_exec_timeout")]
    timeout_secs: u64,
}

/// The v0.5 shape: a result or an error, never both.
#[derive(Serialize)]
struct EvalView {
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    exit_code: i64,
}

/// Evaluate a snippet in a sandbox.
///
/// A thin layer over exec: the template says how to run code
/// (`eval_argv`, `python3 -c` by default) and the snippet is appended.
/// The v0.5 controller reached an agent inside the guest for this; with
/// gVisor there is no agent to reach.
async fn eval_sandbox(
    State(st): State<St>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    Json(req): Json<EvalReq>,
) -> Result<Json<EvalView>, ApiErr> {
    require_sandbox_token(&st, &id, &headers)?;
    let argv = {
        let store = st.store();
        let Some(sb) = store
            .get_sandbox(&id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        else {
            return Err(err(StatusCode::NOT_FOUND, "no such sandbox"));
        };
        let spec = store
            .get_template(&sb.template)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .and_then(|(_r, s)| serde_json::from_str::<serde_json::Value>(&s).ok());
        let mut argv: Vec<String> = spec
            .as_ref()
            .and_then(|v| v.get("eval_argv"))
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_else(|| vec!["/usr/bin/python3".to_string(), "-c".to_string()]);
        argv.push(req.code);
        argv
    };

    let r = run_exec(&st, &id, argv, req.timeout_secs).await?;
    Ok(Json(if r.exit_code == 0 {
        EvalView {
            result: Some(r.stdout),
            error: None,
            exit_code: 0,
        }
    } else {
        EvalView {
            result: None,
            error: Some(if r.stderr.is_empty() {
                r.stdout
            } else {
                r.stderr
            }),
            exit_code: r.exit_code,
        }
    }))
}

#[derive(Serialize)]
struct PingView {
    ok: bool,
    tier: Tier,
    #[serde(skip_serializing_if = "Option::is_none")]
    guest_addr: Option<String>,
    /// How long the check took, so a caller can see a slow node.
    latency_ms: u64,
}

/// Is this sandbox actually answering?
///
/// Reads desired state and then opens a TCP connection to the sandbox's
/// own address. A sandbox that is `hot` in the master's view but not
/// accepting connections reports `ok: false` rather than being taken on
/// trust -- the whole design holds that actual state is observed, never
/// remembered.
async fn ping_sandbox(
    State(st): State<St>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<Json<PingView>, ApiErr> {
    require_sandbox_token(&st, &id, &headers)?;
    let t0 = std::time::Instant::now();
    let (tier, addr) = {
        let store = st.store();
        let Some(sb) = store
            .get_sandbox(&id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        else {
            return Err(err(StatusCode::NOT_FOUND, "no such sandbox"));
        };
        let addr = match (sb.ip.as_deref(), sb.port) {
            (Some(ip), Some(port)) => Some(format!("{ip}:{port}")),
            _ => None,
        };
        (sb.tier, addr)
    };

    let ok = match (&addr, tier) {
        (Some(a), Tier::Hot) => {
            let a = a.clone();
            tokio::time::timeout(Duration::from_secs(3), tokio::net::TcpStream::connect(a))
                .await
                .map(|r| r.is_ok())
                .unwrap_or(false)
        }
        // Anything not hot has no process to answer, which is correct
        // rather than broken.
        _ => false,
    };
    Ok(Json(PingView {
        ok,
        tier,
        guest_addr: addr,
        latency_ms: t0.elapsed().as_millis() as u64,
    }))
}

#[derive(Deserialize)]
struct BranchReq {
    /// Name for the new snapshot.
    tag: String,
    /// Removed in v0.6. Present so the refusal can name it.
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    wait: Option<bool>,
    #[serde(default)]
    live: Option<bool>,
    /// Firecracker diff-snapshot measurement. Refused rather than ignored:
    /// gVisor has no diff snapshots, so the numbers it asked for cannot be
    /// produced, and quietly dropping the flag would have the caller
    /// believe otherwise.
    #[serde(default)]
    diff: Option<bool>,
    #[serde(default)]
    measure_diff: Option<bool>,
}

/// A snapshot as the API serves it, with the v0.5 names the SDKs read:
/// `branched_from` for the sandbox it came from, `created_at_unix` for its
/// age, and `dir` for where the image lives.
#[derive(Serialize)]
struct SnapshotView {
    tag: String,
    node_id: String,
    template: String,
    /// v0.5 name for `from_sandbox`.
    #[serde(skip_serializing_if = "Option::is_none")]
    branched_from: Option<String>,
    created_at_unix: i64,
    dir: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    from_sandbox: Option<String>,
    created_at: i64,
    size_bytes: u64,
}

impl From<SnapshotTag> for SnapshotView {
    fn from(s: SnapshotTag) -> Self {
        Self {
            branched_from: s.from_sandbox.clone(),
            created_at_unix: s.created_at,
            dir: format!("tags/{}", s.tag),
            tag: s.tag,
            node_id: s.node_id,
            template: s.template,
            from_sandbox: s.from_sandbox,
            created_at: s.created_at,
            size_bytes: s.size_bytes,
        }
    }
}

/// Checkpoint a running sandbox into a named tag it can be forked from.
///
/// The parent keeps running: runsc's checkpoint stops the container, so the
/// node restores it from the image it just wrote. Measured on bm that is
/// 33 ms to checkpoint and 45 ms to restore, so the parent pauses for well
/// under a tenth of a second. A branch with no pause at all is an RFC
/// non-goal, and this is the honest version of that trade.
async fn branch_sandbox(
    State(st): State<St>,
    Path(id): Path<String>,
    Json(req): Json<BranchReq>,
) -> Result<(StatusCode, Json<SnapshotView>), ApiErr> {
    // "full" is what a branch is, so it asks for nothing that went away.
    // Anything else is refused rather than ignored, or a caller who asked
    // for a diff believes it got one.
    if req.mode.as_deref().is_some_and(|mode| mode != "full") || req.live == Some(true) {
        return Err(err(
            StatusCode::GONE,
            "mode and live are removed in v0.6. A branch is a checkpoint \
             restored into each child; the parent pauses for about 80 ms. \
             Drop the field for the same call.",
        ));
    }
    if req.wait.is_some() {
        return Err(err(
            StatusCode::GONE,
            "wait is removed in v0.6: branch always waits, and returns the \
             snapshot once the node has written it. Drop the field.",
        ));
    }
    if req.diff == Some(true) || req.measure_diff == Some(true) {
        return Err(err(
            StatusCode::GONE,
            "diff and measure_diff are removed in v0.6: they measured \
             Firecracker diff snapshots, and gVisor has none. The response \
             carries size_bytes for the whole image instead. Drop both.",
        ));
    }
    if !crate::tag::is_safe(&req.tag) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "tag must be a plain name: no slashes, no dots at the start",
        ));
    }

    let (node_id, template, tier, port) = {
        let store = st.store();
        if store
            .get_snapshot(&req.tag)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .is_some()
        {
            return Err(err(
                StatusCode::CONFLICT,
                format!("snapshot {} already exists", req.tag),
            ));
        }
        let Some(sb) = store
            .get_sandbox(&id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        else {
            return Err(err(StatusCode::NOT_FOUND, "no such sandbox"));
        };
        (sb.node_id, sb.template, sb.tier, sb.port)
    };

    // Only a running sandbox has memory to checkpoint.
    if tier != Tier::Hot {
        return Err(err(
            StatusCode::CONFLICT,
            format!("sandbox is {tier}; POST /v1/sandboxes/{id}/touch to bring it hot first"),
        ));
    }

    let result_id = random_token();
    let (tx, rx) = oneshot::channel();
    {
        st.pending_exec.lock().insert(result_id.clone(), tx);
        st.exec_queue
            .lock()
            .entry(node_id.clone())
            .or_default()
            .push_back(Command::Branch {
                id: id.clone(),
                tag: req.tag.clone(),
                result_id: result_id.clone(),
            });
    }
    st.wake_node(&node_id);

    let budget = Duration::from_secs(180);
    let r = match tokio::time::timeout(budget, rx).await {
        Ok(Ok(r)) => r,
        _ => {
            st.pending_exec.lock().remove(&result_id);
            return Err(err(
                StatusCode::GATEWAY_TIMEOUT,
                format!("node {node_id} did not finish the branch within {budget:?}"),
            ));
        }
    };
    if r.exit_code != 0 {
        return Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("branch failed on {node_id}: {}", r.stderr.trim()),
        ));
    }

    // Recorded only once the node has actually written the image, so a fork
    // from this tag cannot race ahead of it.
    let snap = SnapshotTag {
        tag: req.tag,
        node_id,
        template,
        from_sandbox: Some(id),
        created_at: now_secs(),
        size_bytes: r.size_bytes,
        port,
    };
    {
        let store = st.store();
        store
            .put_snapshot(&snap)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        // Which host this image can be restored on. runsc refuses across
        // even a one-week release difference, so the fingerprint is part of
        // what the tag IS, not metadata about it.
        if let Ok(Some(fp)) = store.node_fingerprint(&snap.node_id) {
            let _ = store.set_snapshot_fingerprint(&snap.tag, &fp);
        }
    }
    Ok((StatusCode::CREATED, Json(snap.into())))
}

async fn list_snapshots(State(st): State<St>) -> Result<Json<Vec<SnapshotView>>, ApiErr> {
    let all = st
        .store()
        .list_snapshots()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(all.into_iter().map(Into::into).collect()))
}

#[derive(Serialize)]
struct SnapshotInfo {
    #[serde(flatten)]
    snapshot: SnapshotView,
    /// Sandboxes still forked from it. A tag with children is in use.
    children: u32,
}

async fn snapshot_info(
    State(st): State<St>,
    Path(tag): Path<String>,
) -> Result<Json<SnapshotInfo>, ApiErr> {
    let store = st.store();
    let Some(snap) = store
        .get_snapshot(&tag)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    else {
        return Err(err(StatusCode::NOT_FOUND, format!("no snapshot {tag}")));
    };
    let children = store
        .children_of(&tag)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(SnapshotInfo {
        snapshot: snap.into(),
        children,
    }))
}

/// Drop a tag. Refused while sandboxes are still forked from it: one image
/// serves every child, so removing it under them would strand them.
async fn delete_snapshot(
    State(st): State<St>,
    Path(tag): Path<String>,
) -> Result<StatusCode, ApiErr> {
    let store = st.store();
    let children = store
        .children_of(&tag)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if children > 0 {
        return Err(err(
            StatusCode::CONFLICT,
            format!("{children} sandboxes are still forked from {tag}"),
        ));
    }
    if !store
        .delete_snapshot(&tag)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    {
        return Err(err(StatusCode::NOT_FOUND, format!("no snapshot {tag}")));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Keep the reason a sandbox is not coming up where an operator can see it,
/// and drop it once the sandbox is up.
///
/// A command the node held back for want of memory is kept as the reason
/// but counted as no attempts at all: the node will start it once there is
/// room, and quarantining it would mean nothing ever does.
///
/// And a sandbox this report observes, with no failure against it, has
/// come up: whatever was wrong is over. Before this the error stayed until
/// someone called /retry, and a load test read 5,836 sandboxes that were
/// running fine as failed.
pub(super) fn record_failures(st: &St, req: &PollRequest) -> Result<(), ApiErr> {
    let ise = |e: anyhow::Error| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    let store = st.store();
    let failing: std::collections::HashSet<&str> =
        req.failures.iter().map(|f| f.id.as_str()).collect();
    for id in store.errored_on_node(&req.node_id).map_err(ise)? {
        if !failing.contains(id.as_str()) && req.observed.iter().any(|o| o.id == id) {
            store.clear_sandbox_error(&id).map_err(ise)?;
        }
    }
    for f in &req.failures {
        if f.held_back {
            store.set_sandbox_error(&f.id, &f.error, 0).map_err(ise)?;
            continue;
        }
        store
            .set_sandbox_error(&f.id, &f.error, f.attempts)
            .map_err(ise)?;
        if f.attempts >= MAX_FAILURES {
            tracing::error!(
                sandbox = %f.id, node = %req.node_id, attempts = f.attempts,
                error = %f.error,
                "sandbox quarantined after repeated failures; POST /retry to resume"
            );
        } else {
            tracing::warn!(
                sandbox = %f.id, node = %req.node_id, attempts = f.attempts,
                error = %f.error, "node reported a failing sandbox"
            );
        }
    }
    Ok(())
}

/// Requests the node's gateway forwarded since it last reported.
///
/// The gateway wakes a cold sandbox on its own, so by the time this arrives
/// the sandbox may already be hot against a desired tier that says
/// otherwise. Making Hot the desired tier here, before `diff`, is what stops
/// the next diff from putting it straight back to sleep. And every request
/// moves the idle clock, which nothing did for gateway traffic before: a
/// sandbox serving requests on its port was demoted as idle on schedule.
///
/// Only the node holding the sandbox is believed. Any node can put any id
/// in its report, and one that does not hold the sandbox has no business
/// keeping it awake.
///
/// And traffic only promotes a sandbox this same report shows serving --
/// one that really was woken. A polling node can sit on a report for a
/// whole hold, so traffic can arrive after the node has, quite rightly,
/// demoted the sandbox on a later command once it had gone quiet. Found on
/// bm: a freeze issued 16 s after the last request was undone at once by
/// that request's late report. Late traffic still moves the idle clock;
/// it just does not overrule a demotion that came after it.
pub(super) fn apply_traffic(st: &St, req: &PollRequest) -> Result<(), ApiErr> {
    if req.traffic.is_empty() {
        return Ok(());
    }
    let store = st.store();
    let ise = |e: anyhow::Error| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    for id in &req.traffic {
        let Some(sb) = store.get_sandbox(id).map_err(ise)? else {
            continue;
        };
        if sb.node_id != req.node_id {
            tracing::warn!(sandbox = %id, node = %req.node_id, holder = %sb.node_id, "ignored traffic reported by a node that does not hold the sandbox");
            continue;
        }
        store.touch(id).map_err(ise)?;
        let serving = req
            .observed
            .iter()
            .any(|o| &o.id == id && o.tier.is_serving());
        let asked_for = st
            .explicit_tier
            .lock()
            .get(id)
            .is_some_and(|at| at.elapsed() < EXPLICIT_TIER_HOLD);
        if serving && sb.tier != Tier::Hot && !asked_for {
            tracing::info!(sandbox = %id, node = %req.node_id, was = %sb.tier, "woken by traffic at its gateway");
            store.set_tier(id, Tier::Hot).map_err(ise)?;
            st.metrics.woke(sb.tier);
        }
    }
    Ok(())
}

/// Accept the demotions a node made on its own to make room for a wake.
///
/// Desired state moves to the tier the same report observes, and only ever
/// colder: an eviction that a later request has already undone is not
/// written back over it. Like traffic, only the node holding the sandbox is
/// believed.
pub(super) fn apply_evictions(st: &St, req: &PollRequest) -> Result<(), ApiErr> {
    if req.evicted.is_empty() {
        return Ok(());
    }
    let store = st.store();
    let ise = |e: anyhow::Error| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    for id in &req.evicted {
        let Some(sb) = store.get_sandbox(id).map_err(ise)? else {
            continue;
        };
        if sb.node_id != req.node_id {
            continue;
        }
        let Some(seen) = req.observed.iter().find(|o| &o.id == id) else {
            continue;
        };
        if seen.tier > sb.tier {
            tracing::info!(sandbox = %id, node = %req.node_id, was = %sb.tier, now = %seen.tier, "put to disk by its node to make room for a wake");
            store.set_tier(id, seen.tier).map_err(ise)?;
            st.metrics.evicted.fetch_add(1, Relaxed);
        }
    }
    Ok(())
}

/// Keep what a node says its fingerprint is made of. Every full report
/// carries it, so it is written only when it differs from the last one seen
/// -- once per node per master start, and again only if the host changes.
/// Takes the store the caller already holds: every caller has it locked, and
/// locking it again here would deadlock the poll.
fn record_host(
    st: &St,
    store: &Store,
    node: &str,
    host: &crate::proto::HostInfo,
) -> Result<(), ApiErr> {
    let mut seen = st.metrics.node_hosts.lock();
    if seen.get(node) == Some(host) {
        return Ok(());
    }
    store
        .set_node_host(node, host)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    seen.insert(node.to_string(), host.clone());
    Ok(())
}

/// Desired state minus what the node reports holding. Sync and short, so the
/// store lock is never held across an await.
pub(super) fn diff(st: &St, req: &PollRequest) -> Result<Vec<Command>, ApiErr> {
    let store = st.store();
    store
        .set_node_health(&req.node_id, req.free_memory_mib, req.pressure_some_avg10)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    // An empty list from an early wake report says nothing; keep the last.
    if !req.features.is_empty() {
        st.metrics
            .node_features
            .lock()
            .insert(req.node_id.clone(), req.features.clone());
    }
    if let Some(ts) = &req.templates {
        st.metrics
            .node_templates
            .lock()
            .insert(req.node_id.clone(), ts.clone());
    }
    if let Some(h) = &req.host {
        record_host(st, &store, &req.node_id, h)?;
        // Back on the backend it was asked for: placeable again.
        let mut s = st.switching.lock();
        if s.get(&req.node_id).map(|(k, _)| *k) == backend_of_engine(&h.engine) {
            s.remove(&req.node_id);
        }
    }
    // An upgrade under a registered node changes its fingerprint; it has to
    // leave a pool whose snapshots it can no longer restore.
    if let Some(fp) = req.fingerprint.as_deref().filter(|f| !f.is_empty()) {
        let moved = store
            .refingerprint_node(&req.node_id, fp)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        if let Some((from, to)) = moved {
            tracing::warn!(
                node = %req.node_id, %from, %to, fingerprint = %fp,
                "the node's fingerprint changed; it moved pools so snapshots it can no \
                 longer restore are not scheduled onto it"
            );
        }
    }

    // The node owns addressing, so what it reports is the truth.
    for o in &req.observed {
        if let Some(ip) = o.ip.as_deref() {
            store
                .set_sandbox_ip(&o.id, ip)
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        }
    }

    let desired = store
        .sandboxes_on_node(&req.node_id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut commands = pulls_for(&store, req)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    // Where each tag forked here comes from, looked up once per diff.
    let mut sources: HashMap<String, Option<FetchFrom>> = HashMap::new();
    for want in &desired {
        // Quarantined: keep it in desired state and visible, but stop asking
        // the node to do something that has failed MAX_FAILURES times.
        if want.error_attempts >= MAX_FAILURES {
            continue;
        }
        match req.observed.iter().find(|o| o.id == want.id) {
            // Detached means nothing on any node, so a node not holding it
            // IS the desired state. Without this the tier resurrects itself
            // the instant it is reached, and the whole point of detached --
            // that it costs nothing -- is lost.
            None if want.tier == Tier::Detached => {}
            None => {
                let spec = match store
                    .get_template(&want.template)
                    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
                {
                    Some((_rootfs, spec)) => {
                        let mut v: serde_json::Value =
                            serde_json::from_str(&spec).unwrap_or(serde_json::Value::Null);
                        // The caller's ceiling wins over the template's.
                        // Applied here rather than stored in the template,
                        // so one template can serve sandboxes of different
                        // sizes.
                        if let (Some(mib), Some(obj)) = (want.memory_limit_mib, v.as_object_mut()) {
                            obj.insert("memory_max_mib".to_string(), serde_json::Value::from(mib));
                        }
                        v
                    }
                    None => {
                        // The template was removed after the sandbox was
                        // created. Say so rather than sending a Create the
                        // node can only fail on.
                        tracing::warn!(
                            sandbox = %want.id, template = %want.template,
                            "template is gone; not issuing Create"
                        );
                        continue;
                    }
                };
                let fetch = match want.from_tag.as_deref() {
                    Some(tag) => match sources.get(tag) {
                        Some(f) => f.clone(),
                        None => {
                            let f = fetch_from(&store, tag, &req.node_id)?;
                            sources.insert(tag.to_string(), f.clone());
                            f
                        }
                    },
                    None => None,
                };
                commands.push(Command::Create {
                    id: want.id.clone(),
                    template: want.template.clone(),
                    spec,
                    port: want.port.unwrap_or(DEFAULT_SANDBOX_PORT),
                    volume: want.volume.clone(),
                    net_index: want.net_index,
                    from_tag: want.from_tag.clone(),
                    gateway_port: want.gateway_port,
                    token: want.token.clone(),
                    fetch: fetch.map(Box::new),
                    egress: want.egress.clone(),
                })
            }
            Some(got) => {
                if got.tier != want.tier {
                    commands.push(Command::SetTier {
                        id: want.id.clone(),
                        tier: want.tier,
                    });
                }
                // Rules are what a node reports, not what it was last sent,
                // so a node that restarted gets them back the same way.
                if got.egress.as_ref().is_some_and(|e| *e != want.egress) {
                    commands.push(Command::SetEgress {
                        id: want.id.clone(),
                        egress: want.egress.clone(),
                    });
                }
            }
        }
    }
    // Anything the node holds that we no longer want. Adoption means we only
    // ever destroy what desired state has actually dropped.
    for got in &req.observed {
        if !desired.iter().any(|d| d.id == got.id) {
            // The node also needs to know which slot to reclaim; look it
            // up, since an observed-but-undesired sandbox has no row to
            // read it from once desired state has dropped it.
            commands.push(Command::Destroy {
                id: got.id.clone(),
                net_index: got.net_index,
            });
        }
    }
    // Images of tags that are gone. Deleting a snapshot only removes its
    // record, because the image may be on any node that ever forked it;
    // each node learns here which of the ones it holds are no longer wanted.
    for tag in &req.tags {
        if store
            .get_snapshot(tag)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .is_none()
        {
            commands.push(Command::DropTag { tag: tag.clone() });
        }
    }
    Ok(commands)
}

/// Where `node` fetches `tag`'s image from, when it is not the node that
/// baked it.
///
/// `None` when the image is on `node` already, or when the node that has it
/// is one the master cannot reach -- a node that dials in has no address --
/// in which case placement keeps forks of the tag on that node.
fn fetch_from(store: &Store, tag: &str, node: &str) -> Result<Option<FetchFrom>, ApiErr> {
    let ise = |e: anyhow::Error| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    let Some(snap) = store.get_snapshot(tag).map_err(ise)? else {
        return Ok(None);
    };
    if snap.node_id == node {
        return Ok(None);
    }
    let Some(src) = store.agent_node(&snap.node_id).map_err(ise)? else {
        return Ok(None);
    };
    let expires = crate::tag::fetch_expiry(now_secs());
    Ok(Some(FetchFrom {
        token: crate::tag::fetch_token(&src.secret, tag, node, expires),
        node_id: src.id,
        address: src.address,
        cert_sha256: src.cert_sha256,
        for_node: node.to_string(),
        expires,
    }))
}

// ---- sandboxes ----------------------------------------------------------

/// The sandbox as the API serves it.
///
/// Carries the v0.5 names alongside the v0.6 ones, because both SDKs read
/// the old ones: `snapshot_tag` for the tag it came from, `guest_addr` for
/// where to reach it, `created_at_unix` for its age. Duplication on the
/// wire is cheap; a field an SDK cannot find is not.
#[derive(Serialize)]
pub(super) struct SandboxView {
    id: String,
    node_id: String,
    pool: String,
    template: String,
    tier: Tier,
    /// v0.5 name for the tag this sandbox was forked from.
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot_tag: Option<String>,
    /// v0.5 name for where to reach it, as `ip:port`.
    #[serde(skip_serializing_if = "Option::is_none")]
    guest_addr: Option<String>,
    /// v0.5 name for `created_at`.
    created_at_unix: i64,
    /// The network namespace it runs in, which v0.5 also reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    netns: Option<String>,
    /// The ceiling actually in force: the caller's override when they gave
    /// one, absent when the template's applies.
    #[serde(skip_serializing_if = "Option::is_none")]
    memory_limit_mib: Option<u64>,
    /// What it may reach.
    egress: Egress,
    volume: Option<String>,
    port: Option<u16>,
    /// Host port to reach this sandbox on, from off the node. Absent when
    /// none was free, in which case it is reachable only from its node.
    /// Requests here need the `token` below in `x-sandstorm-sandbox-token`.
    #[serde(skip_serializing_if = "Option::is_none")]
    gateway_port: Option<u16>,
    /// Where to reach it. Set once the node reports the address it gave the
    /// sandbox; absent until then.
    #[serde(skip_serializing_if = "Option::is_none")]
    ip: Option<String>,
    created_at: i64,
    last_active_at: i64,
    /// Why it is not up, when it is not. The console shows this instead of
    /// leaving an operator to guess from a node's log.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
    #[serde(skip_serializing_if = "is_zero")]
    error_attempts: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl From<Sandbox> for SandboxView {
    fn from(s: Sandbox) -> Self {
        // Note the absence of `token`: it is returned once, at creation.
        Self {
            snapshot_tag: s.from_tag.clone(),
            guest_addr: match (s.ip.as_deref(), s.port) {
                (Some(ip), Some(port)) => Some(format!("{ip}:{port}")),
                _ => None,
            },
            created_at_unix: s.created_at,
            netns: Some(format!("sandstorm-{}", s.id)),
            memory_limit_mib: s.memory_limit_mib,
            egress: s.egress,
            id: s.id,
            node_id: s.node_id,
            pool: s.pool,
            template: s.template,
            tier: s.tier,
            volume: s.volume,
            port: s.port,
            gateway_port: s.gateway_port,
            ip: s.ip,
            created_at: s.created_at,
            last_active_at: s.last_active_at,
            last_error: s.last_error,
            error_attempts: s.error_attempts,
        }
    }
}

/// Same body the v0.5 controller took, so the SDKs keep working: fork `n`
/// children from `snapshot_tag`. `template` is the v0.6 way to start one
/// cold; exactly one of the two is required.
#[derive(Deserialize, Default)]
pub(super) struct NewSandbox {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    template: Option<String>,
    /// Fork from this tag. One image serves every child.
    #[serde(default)]
    snapshot_tag: Option<String>,
    /// How many to make. Its presence also selects the v0.5 response
    /// shape, which is a list.
    #[serde(default)]
    n: Option<u32>,
    #[serde(default)]
    pool: Option<String>,
    #[serde(default)]
    node_id: Option<String>,
    /// Only on nodes running this backend, `gvisor` or `firecracker`. For a
    /// template: whether a template can run is a matter of the backend (a
    /// rootfs directory or an ext4 image), and a pool's host groups can mix
    /// them. A fork already runs where its snapshot can be restored.
    #[serde(default)]
    backend: Option<String>,
    #[serde(default)]
    volume: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    /// Per-sandbox memory ceiling in MiB, overriding the template's. The
    /// v0.5 name, which both SDKs send.
    #[serde(default)]
    memory_limit_mib: Option<u64>,
    /// What it may reach, in place of the cluster's default.
    #[serde(default)]
    egress: Option<Egress>,
    /// Accepted and ignored, either way. Every sandbox gets its own
    /// network namespace in v0.6, so `true` asks for what already
    /// happens and `false` asks for something weaker than it will get.
    ///
    /// `false` was refused with a 410 at first, on the reasoning that a
    /// caller should not silently receive something other than it asked
    /// for. That was wrong in practice: both SDKs send `false` by
    /// DEFAULT, so the 410 broke `spawnSandboxes({snapshotTag})` and
    /// `spawn_sandboxes(tag)` -- the plainest call either one makes. A
    /// refusal is right when honouring is impossible and the caller would
    /// be misled; here the caller gets strictly stronger isolation, and
    /// nobody is harmed by not being able to ask for less.
    // Never read, deliberately: the field exists so the wire contract is
    // documented where someone looks for it, not because the value
    // changes anything.
    #[allow(dead_code)]
    #[serde(default)]
    per_child_netns: Option<bool>,
    /// Removed in v0.6. Present only so the refusal can name it.
    #[serde(default)]
    live_fork: Option<bool>,
    #[serde(default)]
    hugepages: Option<bool>,
}

impl NewSandbox {
    /// One sandbox from a template, under an id the caller chose: what the
    /// E2B surface creates.
    pub(super) fn named(id: &str, template: &str, port: u16) -> Self {
        Self {
            id: Some(id.to_string()),
            template: Some(template.to_string()),
            port: Some(port),
            ..Default::default()
        }
    }

    /// With what it may reach decided by the caller.
    pub(super) fn egress(self, egress: Egress) -> Self {
        Self {
            egress: Some(egress),
            ..self
        }
    }
}

pub(super) async fn create_sandbox(
    State(st): State<St>,
    Json(req): Json<NewSandbox>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiErr> {
    // Removed in v0.6. Named in the refusal with what replaces it, as the
    // RFC's compatibility section requires, rather than ignored.
    if req.live_fork == Some(true) || req.hugepages == Some(true) {
        return Err(err(
            StatusCode::GONE,
            "live_fork and hugepages are removed in v0.6; a branch is a \
             checkpoint restored into each child, so there is no memfd to \
             configure. Drop both fields.",
        ));
    }

    // Refused here, not on a node. The node validates again before it
    // touches the filesystem -- that is what stops traversal -- but letting
    // a bad tag through means it is written to the database and then fails
    // on a node with a message about a missing directory. Validating in the
    // handler that joins a path and nowhere else is what the 2026-05-17
    // advisory was about.
    if let Some(t) = req.snapshot_tag.as_deref() {
        if !crate::tag::is_safe(t) {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "snapshot_tag must be a plain name: no path separators, no \
                 leading dot, no \"..\"",
            ));
        }
    }
    let (template, from_tag) = match (&req.template, &req.snapshot_tag) {
        (Some(_), Some(_)) => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "give either template or snapshot_tag, not both",
            ))
        }
        (None, None) => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "one of template or snapshot_tag is required",
            ))
        }
        (Some(t), None) => (t.clone(), None),
        (None, Some(tag)) => (String::new(), Some(tag.clone())),
    };

    let egress = match req.egress.clone() {
        Some(e) => e.canonical().map_err(|e| err(StatusCode::BAD_REQUEST, e))?,
        None => st
            .store()
            .default_egress()
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
    };
    let count = req.n.unwrap_or(1);
    if count == 0 {
        return Err(err(StatusCode::BAD_REQUEST, "n must be at least 1"));
    }
    if count > MAX_FORK_WIDTH {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("n must be at most {MAX_FORK_WIDTH}"),
        ));
    }
    // An explicit id cannot name more than one sandbox.
    if req.id.is_some() && count > 1 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "id cannot be given with n > 1",
        ));
    }
    // A node hides its own seed sandboxes by this prefix, so a sandbox named
    // with it would be invisible to the node that runs it.
    if req
        .id
        .as_deref()
        .is_some_and(|i| i.starts_with(crate::node::WARM_SEED_PREFIX))
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!(
                "ids starting with {} are reserved",
                crate::node::WARM_SEED_PREFIX
            ),
        ));
    }

    let backend: Option<BackendKind> = match req.backend.as_deref().filter(|b| !b.is_empty()) {
        None => None,
        Some(_) if from_tag.is_some() => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "backend is for a template; a fork runs on the backend its snapshot was \
                 taken on",
            ))
        }
        Some(b) => Some(
            b.parse()
                .map_err(|e: anyhow::Error| err(StatusCode::BAD_REQUEST, e.to_string()))?,
        ),
    };

    let store = st.store();

    // Where the children may go. A template goes to its pool. A fork goes
    // to any node whose fingerprint matches the tag's, fetching the image
    // from the node that baked it -- provided that node is one the master
    // connects to, since a node that dials in has no address to fetch from.
    // Otherwise, and for a tag too old to have a recorded fingerprint, it
    // stays on the node that holds the image.
    let (template, place, tag_port) = match &from_tag {
        Some(tag) => {
            let Some(snap) = store
                .get_snapshot(tag)
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            else {
                return Err(err(StatusCode::NOT_FOUND, format!("no snapshot {tag}")));
            };
            let tag_fp = store.snapshot_fingerprint(tag).ok().flatten();
            let fetchable = store
                .agent_node(&snap.node_id)
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
                .is_some();
            let place = match (req.node_id.as_deref(), &tag_fp) {
                (Some(asked), _) if asked == snap.node_id => Place::Node(asked.to_string()),
                (Some(asked), tag_fp) => {
                    // Refused at schedule time rather than left to fail on
                    // the node: a mismatched fingerprint means runsc will not
                    // accept the image, and an unfetchable one means the
                    // image cannot get there at all.
                    let node_fp = store.node_fingerprint(asked).ok().flatten();
                    match (tag_fp, &node_fp) {
                        (Some(t), Some(n)) if t != n => {
                            return Err(err(
                                StatusCode::CONFLICT,
                                format!(
                                    "tag {tag} was baked on {} (fingerprint {t}) and \
                                     cannot be restored on {asked} (fingerprint {n}): \
                                     a runsc checkpoint is not portable across \
                                     differing hosts",
                                    snap.node_id
                                ),
                            ));
                        }
                        (Some(_), Some(_)) if fetchable => Place::Node(asked.to_string()),
                        (Some(_), Some(_)) => {
                            return Err(err(
                                StatusCode::CONFLICT,
                                format!(
                                    "tag {tag} is on node {}, which dials in to the \
                                     master, so no other node can fetch its image and \
                                     it cannot be forked on {asked}. Drop node_id to \
                                     fork it where it lives.",
                                    snap.node_id
                                ),
                            ))
                        }
                        _ => {
                            return Err(err(
                                StatusCode::CONFLICT,
                                format!(
                                    "cannot tell whether {asked} can restore tag {tag}: \
                                     the fingerprint of one of them is not recorded. \
                                     Drop node_id to fork it on {}, where it lives.",
                                    snap.node_id
                                ),
                            ))
                        }
                    }
                }
                (None, Some(fp)) if fetchable => Place::Like {
                    fingerprint: fp.clone(),
                    tag: tag.clone(),
                },
                (None, _) => Place::Node(snap.node_id.clone()),
            };
            (snap.template, place, snap.port)
        }
        None => {
            let Some((_, spec)) = store
                .get_template(&template)
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            else {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    format!("unknown template: {template}"),
                ));
            };
            if is_pending(&spec) {
                return Err(err(
                    StatusCode::CONFLICT,
                    format!("template {template} is still being pulled"),
                ));
            }
            let image_port = serde_json::from_str::<serde_json::Value>(&spec)
                .ok()
                .and_then(|v| v["port"].as_u64())
                .and_then(|p| u16::try_from(p).ok());
            let place = match &req.node_id {
                Some(n) => {
                    // Asked for by name, so refused by name: placed anyway,
                    // it would only fail on the node for want of a rootfs.
                    if let Some(want) = backend {
                        let have = store
                            .node_hosts()
                            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
                            .remove(n)
                            .and_then(|h| backend_of_engine(&h.engine));
                        if have != Some(want) {
                            return Err(err(
                                StatusCode::CONFLICT,
                                format!("node {n} does not run {want}"),
                            ));
                        }
                    }
                    if !st.can_start(n, &template) {
                        return Err(err(
                            StatusCode::CONFLICT,
                            format!(
                                "node {n} has no template {template} for its backend; \
                                 {TEMPLATE_FORMS}"
                            ),
                        ));
                    }
                    Place::Node(n.clone())
                }
                None => Place::Pool(
                    req.pool.clone().unwrap_or_else(|| "default".into()),
                    backend,
                ),
            };
            (template, place, image_port)
        }
    };

    // Every child's node is decided before any is written, so a fork that
    // only partly fits is refused whole rather than half created.
    let placed: Vec<(String, String)> = match &place {
        // A template's children share one node, as they always have.
        Place::Pool(pool, backend) => {
            let n = pick_node(&st, &store, pool, *backend, &template, count).inspect_err(|_| {
                // Counted here rather than from the status code: a 503
                // out of the scheduler means the fleet had no room,
                // which is a different thing from the master being
                // broken.
                st.metrics.creates_refused.fetch_add(1, Relaxed);
            })?;
            // The pool the node is in, which may be one of the asked
            // pool's host groups.
            vec![(n.id, n.pool); count as usize]
        }
        Place::Node(n) => {
            let pool = store
                .list_nodes()
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
                .into_iter()
                .find(|x| &x.id == n)
                .map(|x| x.pool)
                .or_else(|| req.pool.clone())
                .unwrap_or_else(|| "default".into());
            vec![(n.clone(), pool); count as usize]
        }
        // A fork spreads: each child goes to the least loaded matching node,
        // counting the children already placed in this call.
        Place::Like { fingerprint, tag } => {
            let mut extra: HashMap<String, u32> = HashMap::new();
            let mut out = Vec::with_capacity(count as usize);
            for _ in 0..count {
                let n = pick_node_where(
                    &st,
                    &store,
                    &format!("the pool tag {tag} restores in"),
                    |n| &n.fingerprint == fingerprint,
                    &extra,
                    1,
                )
                .inspect_err(|_| {
                    st.metrics.creates_refused.fetch_add(1, Relaxed);
                })?;
                *extra.entry(n.id.clone()).or_default() += 1;
                out.push((n.id, n.pool));
            }
            out
        }
    };

    // All of them or none: a slot or port that runs out partway through
    // must not leave the ones before it behind.
    let batch = store
        .transaction()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut made = Vec::with_capacity(count as usize);
    for (node_id, pool) in &placed {
        // The port is the sandbox's own, inside its namespace, so there is
        // nothing to allocate -- two sandboxes may both listen on 8080.
        // A fork serves where its parent did unless the caller says
        // otherwise: the image holds the program already listening there.
        // A template from an image serves where the image says.
        let port = Some(req.port.or(tag_port).unwrap_or(DEFAULT_SANDBOX_PORT));
        // A host port for the gateway, distinct from `port` above, which is
        // inside the namespace and is 8080 for nearly every sandbox.
        //
        // This revives `allocate_port`, which was written in #324 when ports
        // had to be unique per node, made redundant by per-sandbox
        // namespaces in #328, and then sat unused -- called only by its own
        // tests, with `--port-min`/`--port-max` still on the CLI. The
        // gateway needs exactly what it does, so it is wired to the field it
        // was always right for.
        let gateway_port = store
            .allocate_gateway_port(node_id, st.port_range.0, st.port_range.1)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        if gateway_port.is_none() {
            tracing::warn!(
                node = %node_id,
                range = ?st.port_range,
                "no free gateway port; this sandbox is reachable only from its node"
            );
        }

        // The address plan is the node-local resource now.
        let net_index = match store
            .allocate_net_index(node_id, st.max_net_index)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        {
            Some(i) => Some(i),
            None => {
                return Err(err(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("node {node_id} has no free network slot"),
                ))
            }
        };

        let t = now_secs();
        let sb = Sandbox {
            id: req
                .id
                .clone()
                // 128 bits, not the 48 that `[..12]` gave. A sandbox id is
                // handed to callers and appears in URLs, and RFC 16.5 asks
                // for an id that cannot be guessed or enumerated -- 48 bits
                // is inside reach of an attacker who can make requests.
                .unwrap_or_else(|| format!("sb-{}", random_token())),
            node_id: node_id.clone(),
            pool: pool.clone(),
            template: template.clone(),
            tier: Tier::Hot,
            token: random_token(),
            volume: req.volume.clone(),
            port,
            created_at: t,
            last_active_at: t,
            last_error: None,
            error_attempts: 0,
            net_index,
            ip: None,
            from_tag: from_tag.clone(),
            memory_limit_mib: req.memory_limit_mib,
            gateway_port,
            egress: egress.clone(),
        };
        store
            .insert_sandbox(&sb)
            .map_err(|e| err(StatusCode::CONFLICT, e.to_string()))?;
        let token = sb.token.clone();
        let mut v = serde_json::to_value(SandboxView::from(sb))
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        v["token"] = serde_json::Value::String(token);
        made.push(v);
    }

    batch
        .commit()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    st.metrics
        .creates_placed
        .fetch_add(made.len() as u64, Relaxed);

    // Each node is parked on its stream; this reaches it now.
    let mut woken: Vec<&String> = placed.iter().map(|(n, _)| n).collect();
    woken.sort();
    woken.dedup();
    for n in woken {
        st.wake_node(n);
    }

    // v0.5 callers send `n` and expect a list; a v0.6 caller creating one
    // gets the object it asked for.
    Ok(if req.n.is_some() {
        (StatusCode::CREATED, Json(serde_json::Value::Array(made)))
    } else {
        (
            StatusCode::CREATED,
            Json(made.into_iter().next().expect("count >= 1")),
        )
    })
}

/// Least-loaded admitting node in a pool, whichever of its host groups
/// (`<pool>#<fp8>`) the node is in. Only a snapshot is tied to a host
/// fingerprint; a sandbox started from a template boots on any host, so a
/// group split off by an upgrade or another kind of machine still takes
/// its share of new work.
///
/// Only nodes that can start the template on their own backend: a template
/// is a rootfs directory for runsc and an ext4 image for Firecracker, and a
/// pool that mixes the two may have only one form installed. When no node
/// in the pool has it the create is refused here, saying so, rather than
/// failed on a node about a missing file.
fn pick_node(
    st: &St,
    store: &Store,
    pool: &str,
    backend: Option<BackendKind>,
    template: &str,
    count: u32,
) -> Result<Node, ApiErr> {
    let group = format!("{pool}#");
    let hosts = match backend {
        Some(_) => store
            .node_hosts()
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
        None => HashMap::new(),
    };
    let on_backend = |n: &Node| {
        backend
            .is_none_or(|b| hosts.get(&n.id).and_then(|h| backend_of_engine(&h.engine)) == Some(b))
    };
    let in_pool = |n: &Node| (n.pool == pool || n.pool.starts_with(&group)) && on_backend(n);
    let pool = match backend {
        Some(b) => format!("{pool} on {b}"),
        None => pool.to_string(),
    };
    let up: Vec<Node> = store
        .list_nodes()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .into_iter()
        .filter(|n| n.connected && in_pool(n))
        .collect();
    if !up.is_empty() && !up.iter().any(|n| st.can_start(&n.id, template)) {
        return Err(err(
            StatusCode::CONFLICT,
            format!(
                "no node in pool {pool} has template {template} for its backend; {TEMPLATE_FORMS}"
            ),
        ));
    }
    pick_node_where(
        st,
        store,
        &format!("pool {pool}"),
        |n| in_pool(n) && st.can_start(&n.id, template),
        &HashMap::new(),
        count,
    )
}

/// Where each backend reads a template from, for refusals that name one.
const TEMPLATE_FORMS: &str = "a gVisor node reads <work-dir>/templates/<name>/rootfs \
     (a directory), a Firecracker node <work-dir>/templates/<name>/rootfs.ext4 \
     (an ext4 image)";

/// Least-loaded admitting node among those `fits` accepts. `extra` counts
/// sandboxes already decided for a node in this request but not yet written,
/// so one request placing several does not see the same load each time.
fn pick_node_where(
    st: &St,
    store: &Store,
    what: &str,
    fits: impl Fn(&Node) -> bool,
    extra: &HashMap<String, u32>,
    batch_size: u32,
) -> Result<Node, ApiErr> {
    let nodes = store
        .list_nodes()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut best: Option<(Node, u32)> = None;
    let mut refused: Vec<String> = Vec::new();
    for n in nodes.into_iter().filter(|n| n.connected && fits(n)) {
        if let Some(to) = st.switching_to(&n.id) {
            refused.push(format!("{}: restarting onto {to}", n.id));
            continue;
        }
        let load = store
            .count_sandboxes_on_node(&n.id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            + extra.get(&n.id).copied().unwrap_or(0);
        if let Some(why) = st.admit.refuse(&n, load) {
            refused.push(format!("{}: {why}", n.id));
            continue;
        }
        if n.max_sandboxes > 0
            && u64::from(load) + u64::from(batch_size) > u64::from(n.max_sandboxes)
        {
            refused.push(format!(
                "{}: {load} sandboxes plus batch of {batch_size} exceeds its cap of {}",
                n.id, n.max_sandboxes
            ));
            continue;
        }
        if best.as_ref().is_none_or(|(_, b)| load < *b) {
            best = Some((n, load));
        }
    }
    match best {
        Some((n, _)) => Ok(n),
        None if !refused.is_empty() => Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("every node in {what} is too loaded: {}", refused.join("; ")),
        )),
        None => Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("no connected node in {what}"),
        )),
    }
}

/// Where a create's sandboxes may go.
enum Place {
    /// This node, for every child.
    Node(String),
    /// The least loaded node of this pool, for every child, on this
    /// backend if one was asked for.
    Pool(String, Option<BackendKind>),
    /// Each child to the least loaded node whose fingerprint matches the
    /// tag it is forked from.
    Like { fingerprint: String, tag: String },
}

/// Optional filters and a page for `GET /v1/sandboxes`. With none of them
/// the answer is every sandbox, as it always was.
#[derive(Deserialize, Default)]
struct ListSandboxes {
    tier: Option<Tier>,
    node: Option<String>,
    /// Part of the id or the template name.
    q: Option<String>,
    /// Only those carrying an error.
    #[serde(default)]
    failing: bool,
    limit: Option<usize>,
    #[serde(default)]
    offset: usize,
}

/// Every sandbox, or a filtered page of them.
///
/// A page is what the console asks for: on a 20,000-sandbox fleet the whole
/// list is about 10 MB, too much to fetch every few seconds to show 50 rows.
/// The matching total is in `x-total-count`, so a page can say what it is a
/// page of.
async fn list_sandboxes(
    State(st): State<St>,
    Query(f): Query<ListSandboxes>,
) -> Result<impl IntoResponse, ApiErr> {
    let all = st
        .store()
        .list_sandboxes()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let q =
        f.q.as_deref()
            .map(str::to_lowercase)
            .filter(|q| !q.is_empty());
    let matched: Vec<_> = all
        .into_iter()
        .filter(|s| f.tier.is_none_or(|t| s.tier == t))
        .filter(|s| f.node.as_deref().is_none_or(|n| s.node_id == n))
        .filter(|s| !f.failing || s.last_error.is_some())
        .filter(|s| {
            q.as_deref().is_none_or(|q| {
                s.id.to_lowercase().contains(q) || s.template.to_lowercase().contains(q)
            })
        })
        .collect();
    let total = matched.len();
    let page: Vec<SandboxView> = matched
        .into_iter()
        .skip(f.offset)
        .take(f.limit.unwrap_or(usize::MAX))
        .map(Into::into)
        .collect();
    Ok(([("x-total-count", total.to_string())], Json(page)))
}

async fn get_sandbox(
    State(st): State<St>,
    Path(id): Path<String>,
) -> Result<Json<SandboxView>, ApiErr> {
    let got = st
        .store()
        .get_sandbox(&id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    got.map(|s| Json(s.into()))
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "no such sandbox"))
}

/// Mark a sandbox used. Promotion to hot is what a real request does; this
/// just resets the idle clock so the demoter leaves it alone.
pub(super) async fn touch_sandbox(
    State(st): State<St>,
    Path(id): Path<String>,
) -> Result<Json<SandboxView>, ApiErr> {
    let store = st.store();
    let Some(sb) = store
        .get_sandbox(&id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    else {
        return Err(err(StatusCode::NOT_FOUND, "no such sandbox"));
    };
    store
        .touch(&id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if sb.tier != Tier::Hot {
        store
            .set_tier(&id, Tier::Hot)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        // Promotion is the latency-critical path: a caller is waiting.
        st.wake_node(&sb.node_id);
    }
    let fresh = store
        .get_sandbox(&id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "no such sandbox"))?;
    Ok(Json(fresh.into()))
}

pub(super) async fn delete_sandbox(
    State(st): State<St>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiErr> {
    // Read the node before dropping the row: afterwards there is nothing
    // left to say which node to wake.
    let node_id = {
        let store = st.store();
        let node_id = store
            .get_sandbox(&id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .map(|s| s.node_id);
        store
            .delete_sandbox(&id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        node_id
    };
    st.metrics.deletes.fetch_add(1, Relaxed);
    // The node tears it down when it sees it holds a sandbox desired state
    // no longer lists -- but only once it polls, so wake it. Without this
    // the teardown waits out the node's whole hold: measured on bm, a
    // deleted sandbox kept running for 30 s with a 25 s hold.
    if let Some(n) = node_id {
        st.wake_node(&n);
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---- egress -------------------------------------------------------------

/// A change to a policy: what it names is replaced, the rest stays, so
/// "internet on" from a console does not lose the lists it did not send.
#[derive(Deserialize)]
pub(super) struct EgressChange {
    #[serde(default)]
    pub(super) internet: Option<bool>,
    #[serde(default)]
    pub(super) allow: Option<Vec<String>>,
    #[serde(default)]
    pub(super) deny: Option<Vec<String>>,
}

impl EgressChange {
    fn applied_to(self, e: Egress) -> Result<Egress, ApiErr> {
        Egress {
            internet: self.internet.unwrap_or(e.internet),
            allow: self.allow.unwrap_or(e.allow),
            deny: self.deny.unwrap_or(e.deny),
        }
        .canonical()
        .map_err(|e| err(StatusCode::BAD_REQUEST, e))
    }
}

async fn get_default_egress(State(st): State<St>) -> Result<Json<Egress>, ApiErr> {
    let e = st
        .store()
        .default_egress()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(e))
}

/// For sandboxes created from now on. One that is running keeps its rules:
/// taking the internet from a thousand agents is not what a default means.
async fn set_default_egress(
    State(st): State<St>,
    Json(req): Json<EgressChange>,
) -> Result<Json<Egress>, ApiErr> {
    let store = st.store();
    let e = store
        .default_egress()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let e = req.applied_to(e)?;
    store
        .set_default_egress(&e)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(e))
}

pub(super) async fn set_sandbox_egress(
    State(st): State<St>,
    Path(id): Path<String>,
    Json(req): Json<EgressChange>,
) -> Result<Json<Egress>, ApiErr> {
    let (node_id, e) = {
        let store = st.store();
        let sb = store
            .get_sandbox(&id)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("no sandbox {id}")))?;
        let e = req.applied_to(sb.egress)?;
        store
            .set_sandbox_egress(&id, &e)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        (sb.node_id, e)
    };
    st.wake_node(&node_id);
    Ok(Json(e))
}

// ---- templates ----------------------------------------------------------

/// Record how this node's image pulls stand, and say which to start.
///
/// A pull the node reports running gets no command: a poll with commands is
/// answered at once, so repeating one would spin for as long as it runs.
/// Failed stays failed until someone asks again, as a new attempt.
fn pulls_for(store: &Store, req: &PollRequest) -> anyhow::Result<Vec<Command>> {
    let mut commands = Vec::new();
    let Some(installed) = &req.templates else {
        return Ok(commands);
    };
    let pulls = store.template_pulls()?;
    for t in store.image_templates()? {
        let reported = req
            .pulls
            .iter()
            .find(|p| p.name == t.name && p.attempt == t.attempt);
        if let Some(p) = reported {
            let error = p.error.clone().or_else(|| {
                let c = p.config.as_ref()?;
                (p.done && c.args.is_empty() && !spec_has_args(&t.spec))
                    .then(|| "the image names no program; give args".to_string())
            });
            store.record_pull(&t.name, &req.node_id, p.attempt, p.done, error.as_deref())?;
            if let (Some(c), None) = (&p.config, &error) {
                if is_pending(&t.spec) {
                    store.set_template_spec(&t.name, &finished_spec(&t.spec, c))?;
                }
            }
            continue;
        }
        let failed = pulls
            .iter()
            .any(|p| p.template == t.name && p.node_id == req.node_id && p.error.is_some());
        if !failed && (!installed.contains(&t.name) || is_pending(&t.spec)) {
            commands.push(Command::PullTemplate {
                name: t.name,
                image: t.image,
                attempt: t.attempt,
            });
        }
    }
    Ok(commands)
}

fn is_pending(spec: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(spec)
        .is_ok_and(|v| v["pending"].as_bool() == Some(true))
}

fn spec_has_args(spec: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(spec)
        .is_ok_and(|v| v["args"].as_array().is_some_and(|a| !a.is_empty()))
}

/// What the caller asked for, over what the image says. `ro_binds` is empty
/// on purpose: the default binds the host's `/usr` over the template's own,
/// which is right for a skeleton rootfs and wrong for an image.
fn finished_spec(asked: &str, c: &crate::proto::ImageConfig) -> String {
    let asked: serde_json::Value = serde_json::from_str(asked).unwrap_or_default();
    let args = asked
        .get("args")
        .filter(|a| a.as_array().is_some_and(|a| !a.is_empty()))
        .cloned()
        .unwrap_or_else(|| serde_json::json!(c.args));
    let mut env = c.env.clone();
    env.extend(
        asked["env"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| e.as_str().map(str::to_string)),
    );
    serde_json::json!({
        "args": args,
        "env": env,
        "cwd": c.cwd,
        "ro_binds": [],
        "memory_max_mib": asked["memory_max_mib"],
        "port": asked.get("port").filter(|p| !p.is_null()).cloned().or(c.port.map(Into::into)),
    })
    .to_string()
}

#[derive(Deserialize)]
struct NewImageTemplate {
    name: String,
    image: String,
    #[serde(default = "default_image_memory")]
    memory_max_mib: u64,
    /// In place of the image's `Entrypoint` and `Cmd`.
    #[serde(default)]
    args: Vec<String>,
    /// Added to the image's `Env`.
    #[serde(default)]
    env: Vec<String>,
    /// In place of the lowest port the image exposes.
    #[serde(default)]
    port: Option<u16>,
}

fn default_image_memory() -> u64 {
    512
}

/// Have every node pull an image and keep it as a template. Answers at once;
/// `GET /v1/templates/pulls` says how it goes. Sent again, it is another try.
async fn pull_template(
    State(st): State<St>,
    Json(req): Json<NewImageTemplate>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiErr> {
    if !crate::tag::is_safe(&req.name) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("{:?} is not a plain name", req.name),
        ));
    }
    crate::node::images::Reference::parse(&req.image)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let spec = serde_json::json!({
        "pending": true,
        "args": req.args,
        "env": req.env,
        "memory_max_mib": req.memory_max_mib,
        "port": req.port,
    });
    let store = st.store();
    let fresh = store
        .put_image_template(&req.name, &req.image, &spec.to_string())
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !fresh {
        return Err(err(
            StatusCode::CONFLICT,
            format!("template {} already exists, from something else", req.name),
        ));
    }
    let nodes = store
        .list_nodes()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    drop(store);
    for n in nodes {
        st.wake_node(&n.id);
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "name": req.name, "image": req.image })),
    ))
}

#[derive(Serialize)]
struct PullView {
    name: String,
    image: String,
    /// Whether sandboxes can start from it.
    ready: bool,
    nodes: Vec<PullNodeView>,
}

#[derive(Serialize)]
struct PullNodeView {
    node_id: String,
    /// `pulling`, `ready` or `failed`.
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

async fn list_pulls(State(st): State<St>) -> Result<Json<Vec<PullView>>, ApiErr> {
    let store = st.store();
    let bad = |e: anyhow::Error| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    let pulls = store.template_pulls().map_err(bad)?;
    let views = store
        .image_templates()
        .map_err(bad)?
        .into_iter()
        .map(|t| PullView {
            ready: !is_pending(&t.spec),
            nodes: pulls
                .iter()
                .filter(|p| p.template == t.name)
                .map(|p| PullNodeView {
                    node_id: p.node_id.clone(),
                    state: match (p.done, &p.error) {
                        (_, Some(_)) => "failed",
                        (true, None) => "ready",
                        (false, None) => "pulling",
                    },
                    error: p.error.clone(),
                })
                .collect(),
            name: t.name,
            image: t.image,
        })
        .collect();
    Ok(Json(views))
}

#[derive(Deserialize)]
struct NewTemplate {
    name: String,
    rootfs: String,
    /// OCI spec as JSON. Kept opaque here so a template can carry mounts and
    /// limits without this type having to know about them.
    spec: serde_json::Value,
}

async fn put_template(
    State(st): State<St>,
    Json(req): Json<NewTemplate>,
) -> Result<StatusCode, ApiErr> {
    st.store()
        .put_template(&req.name, &req.rootfs, &req.spec.to_string())
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::CREATED)
}

async fn list_templates(State(st): State<St>) -> Result<Json<Vec<String>>, ApiErr> {
    let names = st
        .store()
        .list_templates()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(names))
}

#[cfg(test)]
mod deferred_tests {
    use super::*;
    use crate::proto::Deferred;

    fn destroy(id: &str) -> Command {
        Command::Destroy {
            id: id.into(),
            net_index: None,
        }
    }

    fn asking(deferred: &[(&str, u64)]) -> PollRequest {
        PollRequest {
            deferred: deferred
                .iter()
                .map(|(id, ms)| Deferred {
                    id: id.to_string(),
                    retry_in_ms: *ms,
                })
                .collect(),
            ..serde_json::from_value(serde_json::json!({
                "node_id": "n1", "observed": [],
                "free_memory_mib": 1024, "pressure_some_avg10": 0.0,
            }))
            .unwrap()
        }
    }

    fn left(req: &PollRequest, waited: Duration) -> Vec<String> {
        let mut commands = vec![destroy("a"), destroy("b")];
        leave_out_deferred(&mut commands, req, waited);
        commands
            .iter()
            .map(|c| crate::node::command_id(c).to_string())
            .collect()
    }

    #[test]
    fn what_a_node_is_putting_off_is_not_handed_back() {
        assert_eq!(left(&asking(&[]), Duration::ZERO), ["a", "b"]);
        assert_eq!(left(&asking(&[("a", 500)]), Duration::ZERO), ["b"]);
    }

    /// The parked poll's second look, once the retry has come up.
    #[test]
    fn it_is_handed_over_once_it_is_due() {
        let req = asking(&[("a", 500), ("b", 2000)]);
        assert_eq!(left(&req, Duration::from_millis(499)), Vec::<String>::new());
        assert_eq!(left(&req, Duration::from_millis(500)), ["a"]);
    }
}
