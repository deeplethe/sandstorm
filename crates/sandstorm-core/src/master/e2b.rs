//! The E2B surface: E2B's REST API, translated onto the native one.
//!
//! RFC section 1 calls E2B compatibility a thin adapter, not a kernel, and
//! this is that adapter. Every call here lands on the same handlers as
//! `/v1`, so placement, admission and the four tiers are untouched; what is
//! added is E2B's shape -- its paths, its field names, `X-API-Key`, its
//! error body -- and the one thing E2B has that Sandstorm does not: envd
//! inside the sandbox, which is given its environment and access token once
//! the sandbox first serves.
//!
//! **States.** E2B has two. `hot` and `frozen` are both `running`: a frozen
//! sandbox answers its next request with no action from the caller, so to
//! an SDK it is running. `snapshot` and `detached` are `paused`: something
//! has to bring them back. The four tiers stay a `/v1` concept.
//!
//! **What is not here.** E2B's template build pipeline, teams, keys and
//! volumes answer 501 and say where the Sandstorm equivalent is, rather
//! than 404 as if the caller had a typo. `beta_fork` is not mapped either:
//! `/v1/sandboxes/:id/branch` is the native operation, and squeezing it
//! into an E2B path is how Cube came to answer 201 with a failure inside.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{json, Value};

use super::api::{self, ApiErr, EgressChange, NewSandbox, SetTierReq, St, MAX_FAILURES};
use super::SANDBOX_TOKEN_HEADER;
use crate::identity::random_token;
use crate::node::PORT_HEADER;
use crate::proto::Egress;
use crate::state::{now_secs, E2bSandbox, Sandbox};
use crate::tier::Tier;

/// The port envd listens on inside every E2B sandbox. The SDKs hard-code it.
pub const ENVD_PORT: u16 = 49983;

/// How long a create waits for envd to take its `/init`, from the sandbox
/// row being written. Covers placement, the node noticing, `runsc run`, and
/// envd binding its port; on bm a cold start of the E2B template is a few
/// seconds, so this is room for a loaded node, not the expected case.
const SERVE_WAIT: Duration = Duration::from_secs(60);

/// Who commands run as when the SDK names nobody, as in E2B's templates.
const DEFAULT_USER: &str = "user";

/// E2B's own default, used when a create names no timeout.
const DEFAULT_TIMEOUT_SECS: i64 = 300;

/// How long the front door trusts a looked-up route. A sandbox never moves
/// node and keeps its gateway port for life, so this only bounds how long a
/// deleted sandbox's route lingers -- and a lingering route is harmless: it
/// carries the old sandbox's token, which the gateway refuses if the port
/// has been given to another.
const ROUTE_TTL: Duration = Duration::from_secs(30);

/// The E2B surface's state: the master's, plus what it is reached as.
pub struct E2b {
    pub(super) st: St,
    /// Sandboxes are `{port}-{id}.{domain}`; the API is `api.{domain}`.
    pub domain: String,
    routes: Mutex<HashMap<String, (Route, Instant)>>,
    client: reqwest::Client,
}

/// Where the front door sends a sandbox's traffic.
#[derive(Debug, Clone)]
pub(super) struct Route {
    /// The node's gateway for this sandbox.
    pub(super) gateway: SocketAddr,
    /// The sandbox's own token, which the gateway demands.
    pub(super) token: String,
    pub(super) traffic_access_token: Option<String>,
}

impl E2b {
    pub fn new(st: St, domain: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            st,
            domain: domain.into().trim_end_matches('.').to_ascii_lowercase(),
            routes: Mutex::new(HashMap::new()),
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(30))
                .build()
                .expect("a reqwest client with no TLS configuration builds"),
        })
    }

    /// Where an E2B sandbox's gateway is. `None` for a sandbox the E2B
    /// surface did not make: the front door adds the sandbox's token on the
    /// caller's behalf, so letting it reach a native sandbox would take
    /// away the token that sandbox's own clients are required to present.
    pub(super) fn route(&self, id: &str) -> Result<Option<Route>, E2bErr> {
        if let Some((r, at)) = self.routes.lock().get(id) {
            if at.elapsed() < ROUTE_TTL {
                return Ok(Some(r.clone()));
            }
        }
        let found = {
            let store = self.st.store();
            match store.get_e2b(id).map_err(E2bErr::internal)? {
                None => None,
                Some((rec, sb)) => sb.gateway_port.and_then(|p| {
                    Some((
                        self.st.node_ip(&store, &sb.node_id)?,
                        p,
                        sb.token,
                        rec.traffic_access_token,
                    ))
                }),
            }
        };
        let Some((ip, port, token, traffic_access_token)) = found else {
            self.forget(id);
            return Ok(None);
        };
        let r = Route {
            gateway: SocketAddr::new(ip, port),
            token,
            traffic_access_token,
        };
        self.routes
            .lock()
            .insert(id.to_string(), (r.clone(), Instant::now()));
        Ok(Some(r))
    }

    pub(super) fn forget(&self, id: &str) {
        self.routes.lock().remove(id);
    }
}

// ---- errors -------------------------------------------------------------

/// E2B's error body, `{"code": 404, "message": "..."}`. The SDKs read
/// `message` out of it; the native `{"error": ...}` would reach the user as
/// an empty string.
#[derive(Debug)]
pub struct E2bErr(pub StatusCode, pub String);

impl E2bErr {
    fn new(code: StatusCode, msg: impl Into<String>) -> Self {
        Self(code, msg.into())
    }
    fn internal(e: impl std::fmt::Display) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
    fn not_found(id: &str) -> Self {
        Self(
            StatusCode::NOT_FOUND,
            format!("sandbox \"{id}\" doesn't exist or you don't have access to it"),
        )
    }
}

impl IntoResponse for E2bErr {
    fn into_response(self) -> Response {
        (
            self.0,
            Json(json!({ "code": self.0.as_u16(), "message": self.1 })),
        )
            .into_response()
    }
}

/// A native refusal, re-shaped. Same status, same words.
impl From<ApiErr> for E2bErr {
    fn from((code, Json(body)): ApiErr) -> Self {
        let msg = body
            .get("error")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| body.to_string());
        Self(code, msg)
    }
}

// ---- routes -------------------------------------------------------------

pub fn router(e: Arc<E2b>) -> Router {
    let open = Router::new().route(
        "/health",
        get(|| async { Json(json!({ "message": "Health check successful" })) }),
    );

    let guarded = Router::new()
        .route("/sandboxes", get(list_running).post(create))
        .route("/v2/sandboxes", get(list_v2).post(create))
        .route("/v2/sandboxes/:id/connect", post(connect))
        .route("/sandboxes/:id", get(detail).delete(kill))
        .route("/sandboxes/:id/connect", post(connect))
        .route("/sandboxes/:id/resume", post(resume))
        .route("/sandboxes/:id/pause", post(pause))
        .route("/sandboxes/:id/network", put(set_network))
        .route("/sandboxes/:id/timeout", post(set_timeout))
        .route("/sandboxes/:id/refreshes", post(refresh))
        .route("/sandboxes/:id/logs", get(logs))
        .route("/sandboxes/:id/metrics", get(not_built))
        .route("/sandboxes/metrics", get(not_built))
        .route("/templates", get(list_templates).post(no_template_builds))
        .route(
            "/templates/*rest",
            get(no_template_builds)
                .post(no_template_builds)
                .patch(no_template_builds)
                .delete(no_template_builds),
        )
        .route("/v2/templates", post(no_template_builds))
        .route("/v3/templates", post(no_template_builds))
        .layer(axum::middleware::from_fn_with_state(e.clone(), require_key));

    open.merge(guarded)
        .fallback(|| async { E2bErr::new(StatusCode::NOT_FOUND, "no such E2B endpoint here") })
        .with_state(e)
}

/// `X-API-Key`, which is the master's API token. E2B's access-token form,
/// `Authorization: Bearer`, is taken too, with the same token.
async fn require_key(State(e): State<Arc<E2b>>, req: Request, next: Next) -> Response {
    let h = req.headers();
    let presented = h
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .or_else(|| {
            h.get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
        })
        .unwrap_or_default()
        .trim();
    if presented.is_empty() {
        return E2bErr::new(
            StatusCode::UNAUTHORIZED,
            "missing API key: set E2B_API_KEY to the master's API token",
        )
        .into_response();
    }
    if !e.st.api_token_matches(presented) {
        return E2bErr::new(StatusCode::UNAUTHORIZED, "invalid API key").into_response();
    }
    next.run(req).await
}

async fn not_built() -> E2bErr {
    E2bErr::new(
        StatusCode::NOT_IMPLEMENTED,
        "sandbox metrics are not available through the E2B API on Sandstorm yet",
    )
}

async fn no_template_builds() -> E2bErr {
    E2bErr::new(
        StatusCode::NOT_IMPLEMENTED,
        "Sandstorm does not run E2B's template build pipeline. Build a rootfs with \
         envd in it and register it with POST /v1/templates on the master; it is then \
         usable here by its name as templateID. See recipes/e2b-codeinterpreter.",
    )
}

// ---- views --------------------------------------------------------------

fn state_of(t: Tier) -> &'static str {
    match t {
        Tier::Hot | Tier::Frozen => "running",
        Tier::Snapshot | Tier::Detached => "paused",
    }
}

fn is_paused(t: Tier) -> bool {
    state_of(t) == "paused"
}

fn rfc3339(secs: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(secs)
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string())
}

/// What the template says a sandbox gets, for the fields E2B reports.
fn sizes(e: &E2b, sb: &Sandbox) -> (u64, u64) {
    let spec =
        e.st.store()
            .get_template(&sb.template)
            .ok()
            .flatten()
            .and_then(|(_, s)| serde_json::from_str::<Value>(&s).ok())
            .unwrap_or(Value::Null);
    let cpus = spec.get("cpu_count").and_then(Value::as_u64).unwrap_or(1);
    let mem = sb
        .memory_limit_mib
        .or_else(|| spec.get("memory_max_mib").and_then(Value::as_u64))
        .unwrap_or(0);
    (cpus, mem)
}

/// `ListedSandbox`, and with `full` the extra fields of `SandboxDetail`.
fn view(e: &E2b, rec: &E2bSandbox, sb: &Sandbox, full: bool) -> Value {
    let (cpus, mem) = sizes(e, sb);
    let mut v = json!({
        "sandboxID": rec.id,
        "clientID": rec.client_id,
        "templateID": rec.template_id,
        "alias": rec.template_id,
        // The creation time, in the list and in the detail alike: Cube's
        // differed between the two (its issue #1926).
        "startedAt": rfc3339(sb.created_at),
        "endAt": rfc3339(rec.end_at),
        "state": state_of(sb.tier),
        "cpuCount": cpus,
        "memoryMB": mem,
        "diskSizeMB": 0,
        "envdVersion": rec.envd_version,
        "metadata": serde_json::from_str::<Value>(&rec.metadata).unwrap_or_else(|_| json!({})),
    });
    if full {
        v["domain"] = json!(e.domain);
        v["envdAccessToken"] = json!(rec.access_token);
        v["trafficAccessToken"] = json!(rec.traffic_access_token);
    }
    v
}

/// The `Sandbox` object that create, connect and resume answer with: what
/// the SDK needs to start talking to envd.
fn handle(e: &E2b, rec: &E2bSandbox) -> Value {
    json!({
        "sandboxID": rec.id,
        "templateID": rec.template_id,
        "alias": rec.template_id,
        "clientID": rec.client_id,
        "envdVersion": rec.envd_version,
        "envdAccessToken": rec.access_token,
        "domain": e.domain,
        "trafficAccessToken": rec.traffic_access_token,
    })
}

fn get_record(e: &E2b, id: &str) -> Result<(E2bSandbox, Sandbox), E2bErr> {
    e.st.store()
        .get_e2b(id)
        .map_err(E2bErr::internal)?
        .ok_or_else(|| E2bErr::not_found(id))
}

// ---- ids ----------------------------------------------------------------

/// An E2B-style sandbox id: lower-case base32, the alphabet E2B's own ids
/// use, so it is a single DNS label and has no `-` to confuse the front
/// door's `{port}-{id}` split.
///
/// 26 characters, 130 bits. E2B's are 20 (100 bits); RFC 16.5 asks for a
/// sandbox id that cannot be guessed and every other id here is 128 bits,
/// so this keeps to that rather than to E2B's length. The SDKs do not check
/// the length.
pub fn new_sandbox_id() -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let raw = hex::decode(random_token() + &random_token()).expect("random_token is hex");
    // 5 bits per character, taken from consecutive bytes.
    let mut out = String::with_capacity(26);
    let (mut acc, mut bits) = (0u32, 0u32);
    for b in raw {
        if out.len() == 26 {
            break;
        }
        acc = (acc << 8) | b as u32;
        bits += 8;
        while bits >= 5 && out.len() < 26 {
            bits -= 5;
            out.push(ALPHABET[((acc >> bits) & 31) as usize] as char);
        }
        acc &= (1 << bits) - 1;
    }
    out
}

/// `clientID`: 8 hex characters, as E2B's are.
fn new_client_id() -> String {
    random_token()[..8].to_string()
}

// ---- envd ---------------------------------------------------------------

/// What envd said when it took its `/init`.
struct Inited {
    version: Option<String>,
}

/// Give envd its environment and access token, waiting for it to serve.
///
/// Sent through the sandbox's own gateway, the way any request reaches it,
/// so a sandbox that went cold since is woken by the same path a user's
/// request would take. Retried until `SERVE_WAIT`: the gateway is not
/// listening until the node has started the sandbox, and envd needs a
/// moment after that to bind.
///
/// Safe to repeat. envd accepts a second `/init` carrying the token it
/// already holds, which is what a resume relies on.
async fn init_envd(e: &E2b, rec: &E2bSandbox) -> Result<Inited, E2bErr> {
    let env_vars: Value = serde_json::from_str(&rec.env_vars).unwrap_or_else(|_| json!({}));
    let body = json!({
        "envVars": env_vars,
        "accessToken": rec.access_token,
        // envd's own default is root. E2B's is `user`, which every SDK call
        // that names no user expects -- `commands.run("id")` says uid 1000.
        "defaultUser": DEFAULT_USER,
        "defaultWorkdir": format!("/home/{DEFAULT_USER}"),
        // No `timestamp`: envd would set the clock from it, which is
        // Firecracker's problem and not gVisor's -- the sandbox reads the
        // host's clock.
    });
    let deadline = Instant::now() + SERVE_WAIT;
    let mut pause = Duration::from_millis(100);
    let mut last = String::from("the sandbox was not placed");
    loop {
        // Re-read each time: the node reports the sandbox's state, and a
        // sandbox that keeps failing to start should end the wait now
        // rather than at the deadline.
        let target = {
            let store = e.st.store();
            let sb = store
                .get_sandbox(&rec.id)
                .map_err(E2bErr::internal)?
                .ok_or_else(|| {
                    E2bErr::new(
                        StatusCode::GONE,
                        format!("sandbox {} was deleted while starting", rec.id),
                    )
                })?;
            if sb.error_attempts >= MAX_FAILURES {
                return Err(E2bErr::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!(
                        "sandbox {} failed to start: {}",
                        rec.id,
                        sb.last_error.as_deref().unwrap_or("no reason recorded")
                    ),
                ));
            }
            if let Some(err) = &sb.last_error {
                last = format!("the node reported: {err}");
            }
            sb.gateway_port
                .and_then(|p| Some((e.st.node_ip(&store, &sb.node_id)?, p, sb.token)))
        };
        if let Some((ip, port, token)) = target {
            let sent = e
                .client
                .post(format!("http://{}/init", SocketAddr::new(ip, port)))
                .header(SANDBOX_TOKEN_HEADER, &token)
                .header(PORT_HEADER, ENVD_PORT.to_string())
                .header("X-Access-Token", &rec.access_token)
                .json(&body)
                .send()
                .await;
            match sent {
                Ok(r) if r.status().is_success() => {
                    let version = r
                        .headers()
                        .get("x-envd-version")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    return Ok(Inited { version });
                }
                // envd refusing the token is not something waiting fixes.
                Ok(r)
                    if r.status() == StatusCode::UNAUTHORIZED
                        && r.headers().contains_key("x-envd-version") =>
                {
                    return Err(E2bErr::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!(
                            "envd in {} refused its access token: {}",
                            rec.id,
                            r.text().await.unwrap_or_default()
                        ),
                    ));
                }
                Ok(r) => {
                    last = format!(
                        "{} from the gateway: {}",
                        r.status(),
                        r.text().await.unwrap_or_default()
                    );
                }
                Err(err) => last = format!("gateway at {ip}:{port}: {err}"),
            }
        }
        if Instant::now() + pause > deadline {
            return Err(E2bErr::new(
                StatusCode::GATEWAY_TIMEOUT,
                format!(
                    "sandbox {} did not start serving envd within {}s; last: {last}",
                    rec.id,
                    SERVE_WAIT.as_secs()
                ),
            ));
        }
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(Duration::from_secs(1));
    }
}

// ---- sandboxes ----------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NewE2bSandbox {
    #[serde(rename = "templateID")]
    template_id: String,
    #[serde(default)]
    timeout: Option<i64>,
    #[serde(default)]
    metadata: Option<HashMap<String, String>>,
    #[serde(default)]
    env_vars: Option<HashMap<String, String>>,
    #[serde(default)]
    auto_pause: Option<bool>,
    // envd always requires an access token.
    #[serde(default)]
    secure: Option<bool>,
    #[serde(default, alias = "allow_internet_access")]
    allow_internet_access: Option<bool>,
    #[serde(default)]
    network: Option<E2bNetwork>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct E2bNetwork {
    #[serde(default)]
    allow_public_traffic: Option<bool>,
    #[serde(default)]
    allow_out: Vec<String>,
    #[serde(default)]
    deny_out: Vec<String>,
}

/// What an E2B sandbox may reach, starting from the cluster's default.
///
/// `allow_internet_access: true` opens nothing the default has closed: older
/// SDKs send it on every create unless told otherwise (2.50 does, 2.62 does
/// not), so it says nothing about what the caller wants, and honouring it
/// would take the decision out of the operator's hands. `false` is always
/// deliberate.
fn e2b_egress(
    default: Egress,
    internet: Option<bool>,
    network: Option<E2bNetwork>,
) -> Result<Egress, String> {
    let network = network.unwrap_or_default();
    Egress {
        internet: default.internet && internet != Some(false),
        allow: [default.allow, network.allow_out].concat(),
        deny: [default.deny, network.deny_out].concat(),
    }
    .canonical()
}

/// A body that names a field replaces it, as on `POST /v1/sandboxes/:id/egress`.
/// `allow_internet_access` is honoured here either way: nothing sends this
/// by default.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct E2bNetworkUpdate {
    #[serde(default, alias = "allow_internet_access")]
    allow_internet_access: Option<bool>,
    #[serde(default)]
    allow_out: Option<Vec<String>>,
    #[serde(default)]
    deny_out: Option<Vec<String>>,
}

async fn set_network(
    State(e): State<Arc<E2b>>,
    Path(id): Path<String>,
    Json(req): Json<E2bNetworkUpdate>,
) -> Result<StatusCode, E2bErr> {
    let _ = api::set_sandbox_egress(
        State(e.st.clone()),
        Path(id),
        Json(EgressChange {
            internet: req.allow_internet_access,
            allow: req.allow_out,
            deny: req.deny_out,
        }),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn create(
    State(e): State<Arc<E2b>>,
    Json(req): Json<NewE2bSandbox>,
) -> Result<(StatusCode, Json<Value>), E2bErr> {
    if req.secure == Some(false) {
        return Err(E2bErr::new(
            StatusCode::NOT_IMPLEMENTED,
            "secure=false is not supported; Sandstorm always requires an envd access token",
        ));
    }
    let private_traffic = req
        .network
        .as_ref()
        .is_some_and(|network| network.allow_public_traffic == Some(false));
    let template = req.template_id.clone();
    let known =
        e.st.store()
            .get_template(&template)
            .map_err(E2bErr::internal)?;
    if known.is_none() {
        return Err(E2bErr::new(
            StatusCode::NOT_FOUND,
            format!(
                "template '{template}' not found; templates are registered with POST /v1/templates"
            ),
        ));
    }
    let default = e.st.store().default_egress().map_err(E2bErr::internal)?;
    let egress = e2b_egress(default, req.allow_internet_access, req.network)
        .map_err(|why| E2bErr::new(StatusCode::BAD_REQUEST, why))?;
    let timeout = req.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS).max(0);
    let rec = E2bSandbox {
        id: new_sandbox_id(),
        client_id: new_client_id(),
        template_id: template.clone(),
        // Generated here and nowhere else, for every sandbox. Cube sent
        // envd no token at all, which left every sandbox's envd open to
        // anyone who could reach it.
        access_token: random_token(),
        traffic_access_token: private_traffic.then(random_token),
        envd_version: String::new(),
        env_vars: serde_json::to_string(&req.env_vars.unwrap_or_default())
            .map_err(E2bErr::internal)?,
        metadata: serde_json::to_string(&req.metadata.unwrap_or_default())
            .map_err(E2bErr::internal)?,
        auto_pause: req.auto_pause.unwrap_or(false),
        end_at: now_secs() + timeout,
    };

    let _ = api::create_sandbox(
        State(e.st.clone()),
        Json(NewSandbox::named(&rec.id, &template, ENVD_PORT).egress(egress)),
    )
    .await?;
    // Bound first: in an `if let` the store guard would live across the
    // await below.
    let recorded = e.st.store().insert_e2b(&rec);
    if let Err(err) = recorded {
        discard(&e, &rec.id).await;
        return Err(E2bErr::internal(err));
    }

    // Created means serving: an SDK goes straight to envd with what this
    // returns, so answering before envd has its token would hand it a
    // sandbox that refuses it.
    match init_envd(&e, &rec).await {
        Ok(inited) => {
            let version = inited.version.unwrap_or_default();
            e.st.store()
                .set_e2b_envd_version(&rec.id, &version)
                .map_err(E2bErr::internal)?;
            let rec = E2bSandbox {
                envd_version: version,
                ..rec
            };
            tracing::info!(sandbox = %rec.id, template = %rec.template_id, envd = %rec.envd_version, "E2B sandbox serving");
            Ok((StatusCode::CREATED, Json(handle(&e, &rec))))
        }
        Err(err) => {
            tracing::warn!(sandbox = %rec.id, error = %err.1, "E2B create failed; removing the sandbox");
            discard(&e, &rec.id).await;
            Err(err)
        }
    }
}

async fn discard(e: &E2b, id: &str) {
    e.forget(id);
    let _ = api::delete_sandbox(State(e.st.clone()), Path(id.to_string())).await;
}

async fn kill(State(e): State<Arc<E2b>>, Path(id): Path<String>) -> Result<StatusCode, E2bErr> {
    get_record(&e, &id)?;
    e.forget(&id);
    api::delete_sandbox(State(e.st.clone()), Path(id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn detail(State(e): State<Arc<E2b>>, Path(id): Path<String>) -> Result<Json<Value>, E2bErr> {
    let (rec, sb) = get_record(&e, &id)?;
    Ok(Json(view(&e, &rec, &sb, true)))
}

/// `GET /sandboxes`: the running ones, as v1 of E2B's API returns.
async fn list_running(State(e): State<Arc<E2b>>) -> Result<Json<Value>, E2bErr> {
    let all = e.st.store().list_e2b().map_err(E2bErr::internal)?;
    Ok(Json(Value::Array(
        all.iter()
            .filter(|(_, sb)| !is_paused(sb.tier))
            .map(|(rec, sb)| view(&e, rec, sb, false))
            .collect(),
    )))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListQuery {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    metadata: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    next_token: Option<String>,
}

/// `GET /v2/sandboxes`, which is what the SDKs' `list` calls: filtered by
/// state and metadata, newest first, paged with `X-Next-Token`.
///
/// `limit` counts sandboxes. Cube passed it on as a node count.
async fn list_v2(
    State(e): State<Arc<E2b>>,
    Query(q): Query<ListQuery>,
) -> Result<Response, E2bErr> {
    let states: Vec<String> = q
        .state
        .as_deref()
        .map(|s| {
            s.split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let wanted_meta: Vec<(String, String)> = q
        .metadata
        .as_deref()
        .map(parse_metadata_query)
        .unwrap_or_default();
    let limit = q.limit.unwrap_or(100).clamp(1, 100);

    let mut all = e.st.store().list_e2b().map_err(E2bErr::internal)?;
    all.retain(|(rec, sb)| {
        let state_ok = states.is_empty() || states.iter().any(|s| s == state_of(sb.tier));
        let meta: HashMap<String, String> = serde_json::from_str(&rec.metadata).unwrap_or_default();
        let meta_ok = wanted_meta.iter().all(|(k, v)| meta.get(k) == Some(v));
        state_ok && meta_ok
    });
    // Newest first, ties by id, so the cursor below is a total order.
    all.sort_by(|(_, a), (_, b)| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    if let Some(tok) = q.next_token.as_deref() {
        let after = parse_cursor(tok).ok_or_else(|| {
            E2bErr::new(
                StatusCode::BAD_REQUEST,
                "nextToken is not one this API issued",
            )
        })?;
        all.retain(|(_, sb)| {
            (std::cmp::Reverse(sb.created_at), sb.id.as_str())
                > (std::cmp::Reverse(after.0), after.1.as_str())
        });
    }
    let more = all.len() > limit;
    all.truncate(limit);
    let body = Value::Array(
        all.iter()
            .map(|(rec, sb)| view(&e, rec, sb, false))
            .collect(),
    );
    let mut res = Json(body).into_response();
    if more {
        if let Some((_, last)) = all.last() {
            if let Ok(v) = format!("{}.{}", last.created_at, last.id).parse() {
                res.headers_mut().insert("x-next-token", v);
            }
        }
    }
    Ok(res)
}

fn parse_cursor(tok: &str) -> Option<(i64, String)> {
    let (t, id) = tok.split_once('.')?;
    Some((t.parse().ok()?, id.to_string()))
}

/// `user=abc&app=prod`, each key and value URL-encoded.
fn parse_metadata_query(s: &str) -> Vec<(String, String)> {
    s.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                Ok(x) => {
                    out.push(x);
                    i += 3;
                    continue;
                }
                Err(_) => out.push(b'%'),
            },
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ConnectReq {
    #[serde(default)]
    timeout: Option<i64>,
}

/// Bring a sandbox back if it is paused, and push its deadline out.
///
/// A running sandbox's deadline is only ever extended here, never cut, as
/// E2B does: connecting to check on a sandbox must not shorten its life.
/// A paused one gets a fresh deadline from now.
async fn connect(
    State(e): State<Arc<E2b>>,
    Path(id): Path<String>,
    body: Option<Json<ConnectReq>>,
) -> Result<(StatusCode, Json<Value>), E2bErr> {
    let req = body.map(|Json(b)| b).unwrap_or_default();
    let (rec, sb) = get_record(&e, &id)?;
    let timeout = req.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS).max(0);
    if !is_paused(sb.tier) {
        let end_at = rec.end_at.max(now_secs() + timeout);
        e.st.store()
            .set_e2b_end_at(&id, end_at)
            .map_err(E2bErr::internal)?;
        return Ok((StatusCode::OK, Json(handle(&e, &rec))));
    }
    bring_back(&e, &rec, timeout).await?;
    Ok((StatusCode::CREATED, Json(handle(&e, &rec))))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ResumeReq {
    #[serde(default)]
    timeout: Option<i64>,
}

/// The older SDKs' resume: like connect, but a running sandbox is a 409.
async fn resume(
    State(e): State<Arc<E2b>>,
    Path(id): Path<String>,
    body: Option<Json<ResumeReq>>,
) -> Result<(StatusCode, Json<Value>), E2bErr> {
    let req = body.map(|Json(b)| b).unwrap_or_default();
    let (rec, sb) = get_record(&e, &id)?;
    if !is_paused(sb.tier) {
        return Err(E2bErr::new(
            StatusCode::CONFLICT,
            format!("sandbox {id} is already running"),
        ));
    }
    bring_back(&e, &rec, req.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS).max(0)).await?;
    Ok((StatusCode::CREATED, Json(handle(&e, &rec))))
}

/// Promote a paused sandbox and wait until envd answers again.
///
/// gVisor restores envd's memory along with everything else, so its token
/// and environment survive a snapshot. A detached sandbox comes back from
/// its archived layer instead, so `/init` is sent again either way -- with
/// the same token, which envd accepts.
async fn bring_back(e: &E2b, rec: &E2bSandbox, timeout: i64) -> Result<(), E2bErr> {
    e.st.store()
        .set_e2b_end_at(&rec.id, now_secs() + timeout)
        .map_err(E2bErr::internal)?;
    e.st.release_pause(&rec.id);
    let _ = api::touch_sandbox(State(e.st.clone()), Path(rec.id.clone())).await?;
    let inited = init_envd(e, rec).await?;
    if let Some(v) = inited.version.filter(|v| *v != rec.envd_version) {
        e.st.store()
            .set_e2b_envd_version(&rec.id, &v)
            .map_err(E2bErr::internal)?;
    }
    Ok(())
}

/// Pause: the snapshot tier, which is the first one with nothing running.
async fn pause(State(e): State<Arc<E2b>>, Path(id): Path<String>) -> Result<StatusCode, E2bErr> {
    let (_, sb) = get_record(&e, &id)?;
    if is_paused(sb.tier) {
        return Err(E2bErr::new(
            StatusCode::CONFLICT,
            format!("sandbox {id} is already paused"),
        ));
    }
    e.st.hold_pause(&id);
    let _ = api::set_tier(
        State(e.st.clone()),
        Path(id),
        Json(SetTierReq {
            tier: Tier::Snapshot,
        }),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct TimeoutReq {
    timeout: i64,
}

/// A new deadline, `timeout` seconds from now. May shorten it.
async fn set_timeout(
    State(e): State<Arc<E2b>>,
    Path(id): Path<String>,
    Json(req): Json<TimeoutReq>,
) -> Result<StatusCode, E2bErr> {
    get_record(&e, &id)?;
    e.st.store()
        .set_e2b_end_at(&id, now_secs() + req.timeout.max(0))
        .map_err(E2bErr::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, Default)]
struct RefreshReq {
    #[serde(default)]
    duration: Option<i64>,
}

/// Keep it alive at least `duration` more seconds. Never shortens.
async fn refresh(
    State(e): State<Arc<E2b>>,
    Path(id): Path<String>,
    body: Option<Json<RefreshReq>>,
) -> Result<StatusCode, E2bErr> {
    let req = body.map(|Json(b)| b).unwrap_or_default();
    let (rec, _) = get_record(&e, &id)?;
    let end_at = rec
        .end_at
        .max(now_secs() + req.duration.unwrap_or(DEFAULT_TIMEOUT_SECS).max(0));
    e.st.store()
        .set_e2b_end_at(&id, end_at)
        .map_err(E2bErr::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Empty, and said to be: the node does not collect a sandbox's output yet.
async fn logs(State(e): State<Arc<E2b>>, Path(id): Path<String>) -> Result<Json<Value>, E2bErr> {
    get_record(&e, &id)?;
    Ok(Json(json!({ "logs": [], "logEntries": [] })))
}

// ---- templates ----------------------------------------------------------

/// Every registered template, by name. A template is usable through E2B
/// only if its rootfs carries envd; nothing here can tell, so all are
/// listed.
async fn list_templates(State(e): State<Arc<E2b>>) -> Result<Json<Value>, E2bErr> {
    let all =
        e.st.store()
            .list_templates_full()
            .map_err(E2bErr::internal)?;
    Ok(Json(Value::Array(
        all.into_iter()
            .map(|(name, spec, created_at)| {
                let spec: Value = serde_json::from_str(&spec).unwrap_or(Value::Null);
                json!({
                    "templateID": name,
                    "buildID": "",
                    "aliases": [name],
                    "names": [name],
                    "cpuCount": spec.get("cpu_count").and_then(Value::as_u64).unwrap_or(1),
                    "memoryMB": spec.get("memory_max_mib").and_then(Value::as_u64).unwrap_or(0),
                    "diskSizeMB": 0,
                    "public": false,
                    "buildCount": 1,
                    "buildStatus": "ready",
                    "spawnCount": 0,
                    "lastSpawnedAt": Value::Null,
                    "createdBy": Value::Null,
                    "createdAt": rfc3339(created_at),
                    "updatedAt": rfc3339(created_at),
                    "envdVersion": "",
                })
            })
            .collect(),
    )))
}

/// The headers a request to envd carries that the front door must not
/// pass on as if the caller had sent them.
pub(super) fn strip_inbound(h: &mut HeaderMap) {
    h.remove(SANDBOX_TOKEN_HEADER);
    h.remove(PORT_HEADER);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> Egress {
        Egress {
            internet: true,
            ..Default::default()
        }
    }

    /// Older SDKs send `true` on every create, so it cannot be what opens a
    /// cluster whose operator has closed it.
    #[test]
    fn the_sdks_true_does_not_overrule_a_closed_cluster() {
        let e = e2b_egress(Egress::default(), Some(true), None).unwrap();
        assert!(!e.internet);
        assert!(e2b_egress(open(), Some(true), None).unwrap().internet);
        assert!(e2b_egress(open(), None, None).unwrap().internet);
    }

    #[test]
    fn false_closes_an_open_cluster() {
        assert!(!e2b_egress(open(), Some(false), None).unwrap().internet);
    }

    #[test]
    fn network_lists_are_added_to_the_default() {
        let net: E2bNetwork =
            serde_json::from_value(json!({"allowOut": ["8.8.8.8"], "denyOut": ["1.2.3.4/32"]}))
                .unwrap();
        let e = e2b_egress(open(), None, Some(net)).unwrap();
        assert_eq!(e.allow, ["8.8.8.8/32"]);
        assert_eq!(e.deny, ["1.2.3.4/32"]);
        let net = E2bNetwork {
            allow_out: vec!["example.com".into()],
            ..Default::default()
        };
        let why = e2b_egress(open(), None, Some(net)).unwrap_err();
        assert!(why.contains("domains are not supported"), "{why}");
    }

    #[test]
    fn ids_are_one_dns_label_with_no_dash() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..256 {
            let id = new_sandbox_id();
            assert_eq!(id.len(), 26, "{id}");
            assert!(
                id.bytes()
                    .all(|c| c.is_ascii_lowercase() || (b'2'..=b'7').contains(&c)),
                "{id}"
            );
            assert!(seen.insert(id));
        }
    }

    #[test]
    fn frozen_is_running_and_snapshot_is_paused() {
        assert_eq!(state_of(Tier::Hot), "running");
        assert_eq!(state_of(Tier::Frozen), "running");
        assert_eq!(state_of(Tier::Snapshot), "paused");
        assert_eq!(state_of(Tier::Detached), "paused");
    }

    #[test]
    fn a_metadata_query_is_url_decoded() {
        assert_eq!(
            parse_metadata_query("user=a%20b&app=prod"),
            vec![
                ("user".to_string(), "a b".to_string()),
                ("app".to_string(), "prod".to_string())
            ]
        );
    }

    #[test]
    fn a_native_error_keeps_its_status_and_words() {
        let e: E2bErr = (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "no connected node"})),
        )
            .into();
        assert_eq!(e.0, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(e.1, "no connected node");
    }
}
