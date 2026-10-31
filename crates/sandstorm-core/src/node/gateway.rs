//! The sandbox gateway: what a sandbox's access token actually guards.
//!
//! RFC 16.5 gives every sandbox a 128-bit token and says "访问它的任何端口都
//! 要带" -- the token is required on any port of that sandbox. Until now
//! there was nothing to require it on: a sandbox listens inside its own
//! namespace and the node's rules drop traffic from siblings and from
//! off-box, so it was reachable only from its own node and the token had
//! nothing to gate. This is the piece that exposes a port and makes the
//! token load-bearing.
//!
//! **Why the gateway is on the node.** Three parts of the RFC pull in
//! different directions and only this placement satisfies all three:
//!
//! - §8 wants port exposure to be "每沙箱一条 DNAT", a plain L4 rule.
//! - §16.5 wants the token checked -- "网关校验" -- which L4 cannot do,
//!   because a DNAT has nowhere to read a credential.
//! - §15.5 says the data plane is "网关 → 节点(直连)" and that a wake
//!   request must never travel through the master.
//!
//! So the gateway cannot be a DNAT (no place for the token) and cannot be
//! on the master (§15.5). It runs on the node, in front of the sandbox.
//!
//! **What that costs: the exposed port is HTTP in v0.6.** A raw TCP stream
//! has nowhere to carry a token, so a protocol-agnostic gateway could not
//! check one -- it would be a DNAT again. HTTP is what the templates serve,
//! and the header is the same `x-sandstorm-sandbox-token` the API already uses.
//! Exposing a non-HTTP port needs a different credential mechanism and is
//! not built.
//!
//! **Cold sandboxes need no tier lookup here.** #327 made a sandbox that is
//! not hot REJECT connections with a TCP reset, so the forward fails at once
//! with connection-refused rather than hanging. The gateway takes that as
//! the sandbox being cold, wakes it (`wake.rs`) and forwards again, so the
//! caller sees a slower response rather than an error. It therefore holds
//! no tier state and cannot go stale against it; a listener stays up for
//! every tier that is on this node, frozen and snapshotted included.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::task::JoinHandle;

use super::wake::Wakeups;
use crate::master::SANDBOX_TOKEN_HEADER;
use crate::proxy;

/// How the response says a request had to wait for a wake, and for how
/// long. For whoever is measuring cold starts; nothing depends on it.
pub const WAKE_HEADER: &str = "x-sandstorm-woke-ms";

/// How long a request waits for a sandbox that is up but not yet listening.
const BOOT_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
/// The longest pause between tries while it waits: at most this long late.
const BOOT_POLL_MAX: std::time::Duration = std::time::Duration::from_millis(50);

/// Everything the gateway needs to serve one sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exposed {
    pub id: String,
    /// Host port the gateway listens on. Allocated per node by the master,
    /// so two sandboxes on one machine never collide.
    pub gateway_port: u16,
    /// The sandbox's address inside its namespace, and the port its process
    /// listens on there -- which may be 8080 for every sandbox, since each
    /// has a namespace of its own.
    pub sandbox_ip: Ipv4Addr,
    pub sandbox_port: u16,
    /// The sandbox's own access token. Travels from the master over the
    /// mTLS control channel and is kept in the bundle so a node restart
    /// does not leave the port open without a credential to check.
    pub token: String,
}

/// What a listener is bound to. A change here needs a new socket; a change
/// to the token alone does not.
#[derive(PartialEq, Eq)]
struct Binding {
    gateway_port: u16,
    sandbox_ip: Ipv4Addr,
    sandbox_port: u16,
}

impl From<&Exposed> for Binding {
    fn from(e: &Exposed) -> Self {
        Self {
            gateway_port: e.gateway_port,
            sandbox_ip: e.sandbox_ip,
            sandbox_port: e.sandbox_port,
        }
    }
}

struct Target {
    id: String,
    sandbox_ip: Ipv4Addr,
    /// Where a request goes when it does not name a port.
    sandbox_port: u16,
    /// Updated in place when the sandbox is re-registered with a new token.
    ///
    /// Restarting the listener instead meant rebinding the same port right
    /// after aborting the task that held it, and under a parallel test suite
    /// that lost often enough to be flaky -- while leaving the sandbox with
    /// no gateway at all when it lost in production. A token change no
    /// longer touches the socket; only a port or address change does.
    token: std::sync::RwLock<String>,
    /// `None` only in tests that exercise the token check alone.
    wake: Option<Arc<Wakeups>>,
}

/// Listeners for the sandboxes currently exposed on this node.
struct Live {
    exposed: Exposed,
    target: Arc<Target>,
    handle: JoinHandle<()>,
    /// Dropped with this entry: aborting `handle` closes the listener and
    /// leaves its connections open, serving the old sandbox.
    _stop: tokio::sync::oneshot::Sender<()>,
}

#[derive(Default)]
pub struct Gateway {
    running: HashMap<String, Live>,
    wake: Option<Arc<Wakeups>>,
}

impl Gateway {
    /// A gateway that answers a cold sandbox's requests with a 503.
    pub fn new() -> Self {
        Self::default()
    }

    /// A gateway that wakes a cold sandbox when a request arrives for it.
    pub fn waking(wake: Arc<Wakeups>) -> Self {
        Self {
            running: HashMap::new(),
            wake: Some(wake),
        }
    }

    /// Bring the listeners in line with what should be exposed.
    ///
    /// Reconciled from the observed sandboxes after every poll rather than
    /// driven by create and destroy events, for the same reason the rest of
    /// the node works that way: a listener that leaked past its sandbox
    /// would leave a port open on the host, and a missed create would leave
    /// a sandbox unreachable with nothing to say why.
    pub async fn reconcile(&mut self, want: &[Exposed]) {
        let wanted: HashMap<&str, &Exposed> = want.iter().map(|e| (e.id.as_str(), e)).collect();

        // Gone, or bound to something else: only these need the socket
        // closed and reopened.
        let mut stale: Vec<String> = Vec::new();
        for (id, live) in self.running.iter() {
            let rebind = match wanted.get(id.as_str()) {
                None => true,
                Some(w) => Binding::from(*w) != Binding::from(&live.exposed),
            };
            if live.handle.is_finished() || rebind {
                stale.push(id.clone());
            }
        }
        for id in stale {
            if let Some(live) = self.running.remove(&id) {
                live.handle.abort();
                // Awaited so the port is released before anything rebinds
                // it. `abort` only requests cancellation.
                let _ = live.handle.await;
                tracing::info!(id = %id, port = live.exposed.gateway_port, "gateway stopped");
            }
        }

        for e in want {
            if let Some(live) = self.running.get_mut(&e.id) {
                // Same socket, possibly a new credential: swap it in place.
                if live.exposed.token != e.token {
                    if let Ok(mut t) = live.target.token.write() {
                        *t = e.token.clone();
                    }
                    live.exposed = e.clone();
                    tracing::info!(id = %e.id, "gateway token rotated");
                }
                continue;
            }
            // Still reported while it is being destroyed, by which time
            // its port may belong to the sandbox that replaced it.
            if self
                .running
                .values()
                .any(|l| l.exposed.gateway_port == e.gateway_port)
            {
                continue;
            }
            self.start(e).await;
        }
    }

    /// Open one sandbox's listener now, if it has none, instead of at the
    /// next `reconcile`. For a sandbox just created: the node reports, and
    /// so reconciles, only after a whole batch of commands, and on bm the
    /// first of 20 forked children ran at 201 ms but could not be reached
    /// until 1,852.
    pub async fn expose(&mut self, e: &Exposed) {
        if self.running.contains_key(&e.id) {
            return;
        }
        // The master hands a port on only once its sandbox is deleted, and
        // a node destroys off its command loop, so the old listener can
        // still be up: on bare metal one in five creates straight after a
        // delete could not bind, and waited a second for the next report.
        // Whatever holds this port now is on its way out.
        let holders: Vec<String> = self
            .running
            .iter()
            .filter(|(_, l)| l.exposed.gateway_port == e.gateway_port)
            .map(|(id, _)| id.clone())
            .collect();
        for id in holders {
            if let Some(live) = self.running.remove(&id) {
                live.handle.abort();
                let _ = live.handle.await;
                tracing::info!(id = %id, port = live.exposed.gateway_port, "gateway stopped: its port was handed on");
            }
        }
        self.start(e).await;
    }

    async fn start(&mut self, e: &Exposed) {
        match spawn_one(e.clone(), self.wake.clone()).await {
            Ok((target, handle, stop)) => {
                tracing::info!(
                    id = %e.id, port = e.gateway_port,
                    upstream = %format!("{}:{}", e.sandbox_ip, e.sandbox_port),
                    "gateway listening"
                );
                self.running.insert(
                    e.id.clone(),
                    Live {
                        exposed: e.clone(),
                        target,
                        handle,
                        _stop: stop,
                    },
                );
            }
            Err(err) => tracing::warn!(
                id = %e.id, port = e.gateway_port, error = %err,
                "gateway could not bind; the sandbox stays unreachable from off-box"
            ),
        }
    }

    pub fn exposed_count(&self) -> usize {
        self.running.len()
    }
}

/// Attempts at a port that is taken, and the wait between them: long enough
/// for a child to reach its `exec`, short enough not to hold up the loop this
/// runs on when the port really is someone else's.
const BIND_TRIES: u32 = 20;
const BIND_PAUSE: std::time::Duration = std::time::Duration::from_millis(5);

/// Bind, allowing for a port that was only just let go.
///
/// A closed listener can outlive its close: a process forked by another
/// thread holds a copy of every descriptor until it reaches `exec`, and a node
/// starts `runsc`, `ip` and `iptables` all the time. Without this, a sandbox
/// given a port just handed on was unreachable until the next report. Seen as
/// `EADDRINUSE` on rebinding, only with other threads forking.
async fn bind_soon(addr: SocketAddr) -> std::io::Result<tokio::net::TcpListener> {
    for _ in 1..BIND_TRIES {
        match tokio::net::TcpListener::bind(addr).await {
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                tokio::time::sleep(BIND_PAUSE).await;
            }
            out => return out,
        }
    }
    tokio::net::TcpListener::bind(addr).await
}

async fn spawn_one(
    e: Exposed,
    wake: Option<Arc<Wakeups>>,
) -> anyhow::Result<(
    Arc<Target>,
    JoinHandle<()>,
    tokio::sync::oneshot::Sender<()>,
)> {
    let bind: SocketAddr = format!("0.0.0.0:{}", e.gateway_port).parse()?;
    let listener = bind_soon(bind).await?;
    let target = Arc::new(Target {
        id: e.id.clone(),
        sandbox_ip: e.sandbox_ip,
        sandbox_port: e.sandbox_port,
        token: std::sync::RwLock::new(e.token.clone()),
        wake,
    });
    let app = axum::Router::new()
        .fallback(proxy)
        .with_state(target.clone());
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .tcp_nodelay(true)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await;
    });
    Ok((target, handle, stop))
}

/// Which port inside the sandbox a request is for, when not the one the
/// sandbox was created with.
///
/// One host port per sandbox, whatever it exposes: an E2B sandbox serves
/// envd on 49983 and whatever the user starts on 8000 or 3000, and giving
/// each its own host port would make port use grow with every server a
/// sandbox starts. The token check is the same for every port, which is
/// what RFC 16.5 asks -- the token is required on any port of the sandbox.
pub const PORT_HEADER: &str = "x-sandstorm-port";

/// How long a connection to a sandbox on this machine may take to open. It
/// is a veth away, so anything slower than this is not coming.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How long one connect to a sandbox waits before it is sent again. The
/// sandbox is on this machine, a veth away, so an answer takes microseconds;
/// one that has not come in 50 ms was dropped, typically because the guest's
/// network was not up yet. Left to the kernel, the SYN is resent after a
/// second: on bare metal every cold Firecracker sandbox answered 1.1 s after
/// it was created, its program listening since 0.57 s.
const CONNECT_ATTEMPT: std::time::Duration = std::time::Duration::from_millis(50);

/// Connect to a sandbox, sending a fresh SYN every `CONNECT_ATTEMPT` until
/// `CONNECT_TIMEOUT`. A refusal still comes back at once: that is how a cold
/// sandbox is told apart.
async fn connect_upstream(addr: SocketAddr) -> std::io::Result<tokio::net::TcpStream> {
    let deadline = std::time::Instant::now() + CONNECT_TIMEOUT;
    loop {
        match proxy::connect(addr, CONNECT_ATTEMPT).await {
            Err(e)
                if e.kind() == std::io::ErrorKind::TimedOut
                    && std::time::Instant::now() < deadline => {}
            r => return r,
        }
    }
}

/// The port a request names, `Err` when it names one badly.
fn requested_port(h: &HeaderMap) -> Result<Option<u16>, ()> {
    let Some(v) = h.get(PORT_HEADER) else {
        return Ok(None);
    };
    match v.to_str().ok().and_then(|s| s.trim().parse::<u16>().ok()) {
        Some(p) if p > 0 => Ok(Some(p)),
        _ => Err(()),
    }
}

/// Held by a response body until it is done: the sandbox stays in flight
/// for the whole of a streamed answer, and counts as used when it ends.
struct Answering {
    _in_flight: Option<super::wake::InFlight>,
    wake: Option<Arc<Wakeups>>,
    id: String,
}

impl Drop for Answering {
    fn drop(&mut self) {
        if let Some(w) = &self.wake {
            w.in_use(&self.id);
        }
    }
}

/// Check the token, then pass the request through.
async fn proxy(State(t): State<Arc<Target>>, req: Request) -> Response {
    let expected = t.token.read().map(|g| g.clone()).unwrap_or_default();
    if !presented_token_matches(req.headers(), &expected) {
        // 401, not 404: the port is open and answering, and pretending
        // otherwise would make a missing header look like a wrong address.
        return (
            StatusCode::UNAUTHORIZED,
            [("content-type", "application/json")],
            format!(
                r#"{{"error":"this sandbox requires its own access token in the {SANDBOX_TOKEN_HEADER} header; it is the `token` field of the response that created the sandbox"}}"#
            ),
        )
            .into_response();
    }
    let port = match requested_port(req.headers()) {
        Ok(p) => p.unwrap_or(t.sandbox_port),
        Err(()) => {
            return (
                StatusCode::BAD_REQUEST,
                [("content-type", "application/json")],
                format!(r#"{{"error":"{PORT_HEADER} must be a port number, 1-65535"}}"#),
            )
                .into_response()
        }
    };
    let upstream = SocketAddr::from((t.sandbox_ip, port));

    // Before connecting, so a demotion that arrives while this request is
    // in flight is deferred rather than pulling the sandbox out from under
    // it.
    if let Some(w) = &t.wake {
        w.saw(&t.id);
    }
    // Held to the end of the answer, the wake included: a sandbox with a
    // request in flight is never put away to make room.
    let in_flight = t.wake.as_ref().map(|w| w.hold(&t.id));

    // A connection first, and the request only once there is one. A cold
    // sandbox refuses (#327), or has no route when it is on disk, before any
    // of the body is read, so it can be woken and the same request sent -- a
    // streamed body cannot be resent.
    let mut conn = connect_upstream(upstream).await;
    let mut woke = None;
    if let (Err(e), Some(w)) = (&conn, &t.wake) {
        if proxy::is_cold(e) {
            match w.wake(&t.id).await {
                Ok(took) => {
                    tracing::info!(id = %t.id, ms = took.as_millis() as u64, "woke on traffic");
                    woke = Some(took);
                    conn = connect_upstream(upstream).await;
                    // Up and still refusing: the program is still starting
                    // -- a sandbox created a moment ago, typically. Wait for
                    // it a while rather than answer 502 to a request that
                    // would have succeeded a second later.
                    //
                    // Short pauses: a refused connect to the sandbox's own
                    // address costs microseconds, and every pause is time
                    // the caller waits after the program is up. Backing off
                    // to 500 ms held the first answer of an app64 on bare
                    // metal 300-370 ms past the moment it listened.
                    let deadline = std::time::Instant::now() + BOOT_WAIT;
                    let mut pause = std::time::Duration::from_millis(5);
                    while conn.as_ref().is_err_and(proxy::is_cold)
                        && std::time::Instant::now() < deadline
                    {
                        tokio::time::sleep(pause).await;
                        pause = (pause * 2).min(BOOT_POLL_MAX);
                        conn = connect_upstream(upstream).await;
                    }
                }
                Err(e) => {
                    tracing::warn!(id = %t.id, error = %format!("{e:#}"), "could not wake on traffic");
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        [("content-type", "application/json"), ("retry-after", "5")],
                        serde_json::json!({
                            "error": format!("sandbox {} is cold and could not be woken: {e:#}", t.id)
                        })
                        .to_string(),
                    )
                        .into_response();
                }
            }
        }
    }

    // Every 502 says why in the log as well as to the caller: at 80 per
    // node and 8 req/s they were 7% of requests, and finding out which step
    // failed took matching request logs against the node's by hand.
    let bad_gateway = |why: &str| {
        tracing::warn!(
            id = %t.id,
            woke_ms = woke.map(|d| d.as_millis() as u64),
            error = %why,
            "answered 502"
        );
    };

    let stream = match conn {
        Ok(s) => s,
        // A sandbox that is not hot REJECTs with a reset (#327), so this is
        // the ordinary cold case and arrives in milliseconds rather than as
        // a timeout. A gateway that wakes has already tried that and only
        // gets here if the sandbox came up and still refused. Said plainly,
        // because a bare 502 would look like the sandbox had crashed.
        Err(e) if proxy::is_cold(&e) && woke.is_some() => {
            bad_gateway(&format!(
                "woken, but connections still refused after {BOOT_WAIT:?}: {e:#}"
            ));
            return (
                StatusCode::BAD_GATEWAY,
                [("content-type", "application/json")],
                format!(
                    r#"{{"error":"sandbox {} was woken but nothing is listening on port {port} inside it"}}"#,
                    t.id
                ),
            )
                .into_response();
        }
        Err(e) if proxy::is_cold(&e) && t.wake.is_some() => {
            // Awake and refusing: the sandbox is up, the port is not.
            return (
                StatusCode::BAD_GATEWAY,
                [("content-type", "application/json")],
                format!(
                    r#"{{"error":"nothing is listening on port {port} inside sandbox {}"}}"#,
                    t.id
                ),
            )
                .into_response();
        }
        Err(e) if proxy::is_cold(&e) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [("content-type", "application/json")],
                format!(
                    r#"{{"error":"sandbox {} is not serving: it is frozen, snapshotted or detached. Promote it with POST /v1/sandboxes/{}/touch and retry."}}"#,
                    t.id, t.id
                ),
            )
                .into_response();
        }
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
            return (
                StatusCode::GATEWAY_TIMEOUT,
                format!("sandbox {} did not accept a connection: {e}", t.id),
            )
                .into_response();
        }
        Err(e) => {
            bad_gateway(&format!("connecting: {e:#}"));
            return (StatusCode::BAD_GATEWAY, format!("{e}")).into_response();
        }
    };

    let (mut parts, body) = req.into_parts();
    // Not forwarded: they address the gateway and mean nothing to the
    // sandbox, which should never see a credential it could log or leak.
    parts.headers.remove(SANDBOX_TOKEN_HEADER);
    parts.headers.remove(PORT_HEADER);
    // HTTP/1.1 requires a Host, and Go's server -- envd is one -- refuses a
    // request without it. The caller's is kept when there is one, since a
    // dev server may check it.
    if !parts.headers.contains_key(axum::http::header::HOST) {
        if let Ok(v) = upstream.to_string().parse() {
            parts.headers.insert(axum::http::header::HOST, v);
        }
    }
    if let Some(pq) = parts.uri.path_and_query().cloned() {
        parts.uri = pq.as_str().parse().unwrap_or_default();
    }

    match proxy::send(stream, Request::from_parts(parts, body)).await {
        Ok(res) => {
            // It has just been working, however long ago the request
            // arrived.
            if let Some(w) = &t.wake {
                w.in_use(&t.id);
            }
            let (mut parts, body) = res.into_parts();
            if let Some(took) = woke {
                if let Ok(v) = took.as_millis().to_string().parse() {
                    parts.headers.insert(WAKE_HEADER, v);
                }
            }
            let held = Answering {
                _in_flight: in_flight,
                wake: t.wake.clone(),
                id: t.id.clone(),
            };
            Response::from_parts(parts, proxy::Holding::wrap(body, held))
        }
        Err(e) => {
            bad_gateway(&format!("forwarding: {e:#}"));
            (StatusCode::BAD_GATEWAY, format!("{e}")).into_response()
        }
    }
}

fn presented_token_matches(headers: &HeaderMap, expected: &str) -> bool {
    let presented = headers
        .get(SANDBOX_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    crate::auth::secrets_match(presented, expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(id: &str, port: u16) -> Exposed {
        Exposed {
            id: id.into(),
            gateway_port: port,
            sandbox_ip: "10.86.0.2".parse().unwrap(),
            sandbox_port: 8080,
            token: "a".repeat(32),
        }
    }

    #[test]
    fn a_missing_header_does_not_match() {
        let h = HeaderMap::new();
        assert!(!presented_token_matches(&h, &"a".repeat(32)));
    }

    #[test]
    fn the_wrong_token_does_not_match() {
        let mut h = HeaderMap::new();
        h.insert(SANDBOX_TOKEN_HEADER, "b".repeat(32).parse().unwrap());
        assert!(!presented_token_matches(&h, &"a".repeat(32)));
        // A prefix must not pass either -- the comparison is padded, so a
        // short token cannot match on its first bytes.
        h.insert(SANDBOX_TOKEN_HEADER, "a".repeat(16).parse().unwrap());
        assert!(!presented_token_matches(&h, &"a".repeat(32)));
    }

    #[test]
    fn the_right_token_matches() {
        let mut h = HeaderMap::new();
        h.insert(SANDBOX_TOKEN_HEADER, "a".repeat(32).parse().unwrap());
        assert!(presented_token_matches(&h, &"a".repeat(32)));
    }

    /// Two ports below the ephemeral range.
    ///
    /// Three wrong versions came first and the reasons are worth keeping,
    /// because each looked right:
    ///
    /// 1. Hard-coded 38011/38012. Linux hands out ephemeral ports from
    ///    32768, so a parallel test binding `:0` can be given one of them.
    /// 2. Asking the kernel with `:0` and releasing it. Worse: that returns
    ///    a port inside the ephemeral range by definition, and the observed
    ///    failure was `port=57621 Address already in use` after something
    ///    took it between the release and the rebind.
    /// 3. Fixed ports under 32768, which fixed the port choice but not the
    ///    flake -- because the flake was never about port choice. It was the
    ///    rebind itself, and the fix was to stop rebinding when only the
    ///    token changes.
    ///
    /// These stay under 32768 so `:0` never collides with them, on Linux or
    /// macOS. Do not "simplify" them back to `:0`. The wake tests use
    /// 28461-28469 for the same reason, the E2B test 28471, and a kept
    /// connection's test 28485.
    async fn free_pair() -> (u16, u16) {
        let a = tokio::net::TcpListener::bind(("127.0.0.1", 28451u16))
            .await
            .expect("28451 free");
        let b = tokio::net::TcpListener::bind(("127.0.0.1", 28452u16))
            .await
            .expect("28452 free");
        let (pa, pb) = (
            a.local_addr().unwrap().port(),
            b.local_addr().unwrap().port(),
        );
        drop(a);
        drop(b);
        (pa, pb)
    }

    #[tokio::test]
    async fn a_port_let_go_a_moment_ago_can_be_bound() {
        let held = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = held.local_addr().unwrap();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            drop(held);
        });
        bind_soon(addr).await.unwrap();
    }

    #[tokio::test]
    async fn a_port_someone_keeps_is_given_up_on() {
        let held = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = held.local_addr().unwrap();
        let t0 = std::time::Instant::now();
        let err = bind_soon(addr).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(1),
            "{:?}",
            t0.elapsed()
        );
    }

    /// A plain HTTP server on `port` that answers every request with "hi".
    /// What a sandbox looks like once it is woken.
    fn serve_hi(port: u16) {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        std::thread::spawn(move || {
            for mut s in l.incoming().flatten() {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nhi",
                );
            }
        });
    }

    /// Answers each request `after` it arrives.
    fn serve_slowly(port: u16, after: std::time::Duration) {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        std::thread::spawn(move || {
            for mut s in l.incoming().flatten() {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                std::thread::sleep(after);
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nhi",
                );
            }
        });
    }

    async fn get(port: u16) -> reqwest::Response {
        reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/x"))
            .header(SANDBOX_TOKEN_HEADER, "a".repeat(32))
            .send()
            .await
            .unwrap()
    }

    /// The whole point: a request for a cold sandbox wakes it and gets the
    /// sandbox's answer, not a 503 telling the caller to go and wake it.
    #[tokio::test]
    async fn a_request_for_a_cold_sandbox_wakes_it() {
        let (gw_port, upstream) = (28461u16, 28462u16);
        let woken = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let w = {
            let woken = woken.clone();
            Arc::new(Wakeups::new(move |_, _, _| {
                // Nothing listens until the first wake, which is how a
                // frozen sandbox behind #327's REJECT looks from here.
                if woken.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    serve_hi(upstream);
                }
                Ok(())
            }))
        };
        let mut g = Gateway::waking(w.clone());
        let mut e = spec("sb-cold", gw_port);
        e.sandbox_ip = "127.0.0.1".parse().unwrap();
        e.sandbox_port = upstream;
        g.reconcile(&[e]).await;

        let r = get(gw_port).await;
        assert_eq!(r.status(), 200);
        assert!(
            r.headers().contains_key(WAKE_HEADER),
            "a woken request says so"
        );
        assert_eq!(r.text().await.unwrap(), "hi");
        assert_eq!(woken.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Now it is up: no second wake, and nothing says there was one.
        let r = get(gw_port).await;
        assert_eq!(r.status(), 200);
        assert!(!r.headers().contains_key(WAKE_HEADER));
        assert_eq!(woken.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Both requests are reported to the master, once.
        assert_eq!(w.take_traffic(), vec!["sb-cold".to_string()]);
        g.reconcile(&[]).await;
    }

    /// A program that starts listening a while after its sandbox is up is
    /// answered soon after it does. The retries used to back off to 500 ms,
    /// which held one listening at 400 ms until 750.
    #[tokio::test]
    async fn a_program_still_starting_is_answered_soon_after_it_listens() {
        let (gw_port, upstream) = (28473u16, 28474u16);
        let w = Arc::new(Wakeups::new(move |_, _, _| {
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(400));
                serve_hi(upstream);
            });
            Ok(())
        }));
        let mut g = Gateway::waking(w);
        let mut e = spec("sb-starting", gw_port);
        e.sandbox_ip = "127.0.0.1".parse().unwrap();
        e.sandbox_port = upstream;
        g.reconcile(&[e]).await;

        let t0 = std::time::Instant::now();
        let r = get(gw_port).await;
        let took = t0.elapsed();
        assert_eq!(r.status(), 200);
        assert!(
            took < std::time::Duration::from_millis(600),
            "answered {took:?} after asking, for a program up at 400 ms"
        );
        g.reconcile(&[]).await;
    }

    /// A port the master has handed to a new sandbox goes to it, even while
    /// the old sandbox's listener is still up because its destroy has not
    /// finished; and the old one, still reported, does not take it back.
    #[tokio::test]
    async fn a_port_handed_on_goes_to_the_new_sandbox() {
        let mut g = Gateway::new();
        let port = 28477u16;
        let old = spec("sb-old", port);
        g.reconcile(std::slice::from_ref(&old)).await;
        assert!(g.running.contains_key("sb-old"));

        let new = spec("sb-new", port);
        g.expose(&new).await;
        assert!(
            g.running.contains_key("sb-new"),
            "the new sandbox has the port"
        );
        assert!(!g.running.contains_key("sb-old"));

        g.reconcile(&[old, new]).await;
        assert!(g.running.contains_key("sb-new"));
        assert!(!g.running.contains_key("sb-old"), "the old one is skipped");
        g.reconcile(&[]).await;
    }

    /// A sandbox is spared from being put away while a request to it is in
    /// flight, however long that takes, and is not once it has answered.
    #[tokio::test]
    async fn a_sandbox_is_spared_while_it_is_answering() {
        let (gw_port, upstream) = (28465u16, 28466u16);
        serve_slowly(upstream, std::time::Duration::from_millis(1500));
        let w = Arc::new(Wakeups::new(|_, _, _| Ok(())));
        let mut g = Gateway::waking(w.clone());
        let mut e = spec("sb-slow", gw_port);
        e.sandbox_ip = "127.0.0.1".parse().unwrap();
        e.sandbox_port = upstream;
        g.reconcile(&[e]).await;

        let request = tokio::spawn(get(gw_port));
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(w.protected("sb-slow", std::time::Instant::now()));
        assert_eq!(request.await.unwrap().status(), 200);
        assert!(!w.protected("sb-slow", std::time::Instant::now()));
        g.reconcile(&[]).await;
    }

    #[tokio::test]
    async fn a_streamed_answer_stays_protected_after_headers_and_releases_on_finish_or_cancel() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let upstream = tokio::net::TcpListener::bind(("127.0.0.1", 28481u16))
            .await
            .unwrap();
        let (release_first, first_released) = tokio::sync::oneshot::channel::<()>();
        let (release_second, second_released) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            for release in [first_released, second_released] {
                let (mut stream, _) = upstream.accept().await.unwrap();
                let mut request = [0u8; 4096];
                assert!(stream.read(&mut request).await.unwrap() > 0);
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n")
                    .await
                    .unwrap();
                let _ = release.await;
                let _ = stream.write_all(b"hi").await;
            }
        });

        let wake = Arc::new(Wakeups::new(|_, _, _| Ok(())));
        let mut gateway = Gateway::waking(wake.clone());
        let mut exposed = spec("sb-stream", 28480);
        exposed.sandbox_ip = "127.0.0.1".parse().unwrap();
        exposed.sandbox_port = 28481;
        gateway.reconcile(&[exposed]).await;

        let first = get(28480).await;
        assert_eq!(first.status(), 200);
        assert!(wake.protected("sb-stream", std::time::Instant::now()));
        release_first.send(()).unwrap();
        assert_eq!(first.text().await.unwrap(), "hi");
        assert!(!wake.protected("sb-stream", std::time::Instant::now()));

        let second = get(28480).await;
        assert_eq!(second.status(), 200);
        assert!(wake.protected("sb-stream", std::time::Instant::now()));
        drop(second);
        for _ in 0..100 {
            if !wake.protected("sb-stream", std::time::Instant::now()) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(!wake.protected("sb-stream", std::time::Instant::now()));
        let _ = release_second.send(());
        server.await.unwrap();
        gateway.reconcile(&[]).await;
    }

    #[tokio::test]
    async fn a_wake_the_node_refuses_is_a_503_with_the_reason() {
        let gw_port = 28463u16;
        let w = Arc::new(Wakeups::new(|_, _, _| {
            anyhow::bail!("node held it back: 100 MiB free, below its own 512 MiB floor")
        }));
        let mut g = Gateway::waking(w);
        let mut e = spec("sb-full", gw_port);
        e.sandbox_ip = "127.0.0.1".parse().unwrap();
        // Nothing will ever listen here.
        e.sandbox_port = 28464;
        g.reconcile(&[e]).await;

        let r = get(gw_port).await;
        assert_eq!(r.status(), 503);
        assert!(r.headers().contains_key("retry-after"));
        let body = r.text().await.unwrap();
        assert!(body.contains("512 MiB floor"), "{body}");
        g.reconcile(&[]).await;
    }

    /// One host port serves every port in the sandbox: the header picks
    /// which, and never reaches the sandbox itself.
    #[tokio::test]
    async fn the_port_header_reaches_another_port_in_the_sandbox() {
        let (gw_port, default_port, other_port) = (28467u16, 28468u16, 28469u16);
        serve_hi(other_port);
        let mut g = Gateway::new();
        let mut e = spec("sb-ports", gw_port);
        e.sandbox_ip = "127.0.0.1".parse().unwrap();
        // Nothing listens on the default port.
        e.sandbox_port = default_port;
        g.reconcile(&[e]).await;

        let r = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{gw_port}/x"))
            .header(SANDBOX_TOKEN_HEADER, "a".repeat(32))
            .header(PORT_HEADER, other_port.to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.text().await.unwrap(), "hi");

        let r = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{gw_port}/x"))
            .header(SANDBOX_TOKEN_HEADER, "a".repeat(32))
            .header(PORT_HEADER, "not-a-port")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        g.reconcile(&[]).await;
    }

    #[tokio::test]
    async fn reconcile_starts_stops_and_replaces() {
        let mut g = Gateway::new();
        let (pa, pb) = free_pair().await;
        let a = spec("sb-a", pa);
        let b = spec("sb-b", pb);
        assert_ne!(a.gateway_port, b.gateway_port);

        g.reconcile(&[a.clone(), b.clone()]).await;
        assert_eq!(g.exposed_count(), 2);

        // Dropping one stops only that listener.
        g.reconcile(std::slice::from_ref(&a)).await;
        assert_eq!(g.exposed_count(), 1);

        // A changed token must replace the listener rather than be ignored:
        // otherwise a re-registered sandbox would keep accepting the old
        // credential.
        let mut a2 = a.clone();
        a2.token = "c".repeat(32);
        g.reconcile(&[a2.clone()]).await;
        assert_eq!(
            g.exposed_count(),
            1,
            "replacing a listener rebinds the same port, which races the \
             abort unless it is awaited"
        );
        assert_eq!(
            g.running.get("sb-a").map(|l| l.exposed.token.clone()),
            Some(a2.token)
        );

        g.reconcile(&[]).await;
        assert_eq!(g.exposed_count(), 0);
    }

    /// A port is handed on once its sandbox is deleted, and a client keeps
    /// its connection to that port. If the connection stayed with the old
    /// sandbox, the next one's token would be refused there, and an E2B
    /// create would wait out its minute on a 401.
    #[tokio::test]
    async fn a_kept_connection_does_not_outlive_its_sandbox() {
        let port = 28485u16;
        let mut g = Gateway::new();
        let mut old = spec("sb-old", port);
        // Nothing listens there: the answer is about the sandbox, at once.
        old.sandbox_ip = "127.0.0.1".parse().unwrap();
        old.sandbox_port = 1;
        let mut new = spec("sb-new", port);
        new.sandbox_ip = old.sandbox_ip;
        new.sandbox_port = old.sandbox_port;
        new.token = "c".repeat(32);
        let client = reqwest::Client::new();
        let get = |token: String| {
            client
                .get(format!("http://127.0.0.1:{port}/"))
                .header(SANDBOX_TOKEN_HEADER, token)
                .send()
        };

        g.reconcile(std::slice::from_ref(&old)).await;
        assert_ne!(get(old.token.clone()).await.unwrap().status(), 401);

        g.reconcile(std::slice::from_ref(&new)).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_ne!(get(new.token.clone()).await.unwrap().status(), 401);
        g.reconcile(&[]).await;
    }
}
