//! The master connecting to a node, rather than the node dialing in.
//!
//! The node here is a stand-in: a TLS server that speaks the agent's two
//! routes with a real certificate and pairing code from `AgentIdentity`,
//! and keeps an in-memory list of "sandboxes" instead of calling runsc. That
//! is enough to check what matters on the master's side -- that it pins the
//! certificate, presents the secret, learns the node, and then pushes it the
//! same commands a polling node would be handed.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use parking_lot::Mutex;
use sandstorm_core::identity::Ca;
use sandstorm_core::master::{router, MasterState};
use sandstorm_core::node::AgentIdentity;
use sandstorm_core::proto::{
    AgentHello, Command, HostInfo, ObservedSandbox, PollRequest, SyncRequest, SyncResponse,
};
use sandstorm_core::state::Store;
use sandstorm_core::tier::Tier;
use tempfile::TempDir;

#[derive(Default)]
struct Fake {
    held: HashMap<String, ObservedSandbox>,
    batches: Vec<Vec<Command>>,
    syncs: u32,
}

struct FakeNode {
    secret: String,
    state: Mutex<Fake>,
}

fn authed(n: &FakeNode, h: &HeaderMap) -> bool {
    h.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {}", n.secret))
}

async fn fake_hello(
    State(n): State<Arc<FakeNode>>,
    h: HeaderMap,
) -> Result<Json<AgentHello>, StatusCode> {
    if !authed(&n, &h) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(Json(AgentHello {
        node_id: "fake-1".into(),
        fingerprint: "fp-fake".into(),
        max_sandboxes: 0,
        host: Some(HostInfo {
            engine: "runsc version fake".into(),
            kernel: "6.1.0-fake".into(),
            cpu_vendor: "GenuineFake".into(),
            cpu_model: "Fake CPU".into(),
            cpu_flags: "fpu sse2".into(),
        }),
    }))
}

async fn fake_sync(
    State(n): State<Arc<FakeNode>>,
    h: HeaderMap,
    Json(req): Json<SyncRequest>,
) -> Result<Json<SyncResponse>, StatusCode> {
    if !authed(&n, &h) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let mut s = n.state.lock();
    s.syncs += 1;
    if let Some(cmds) = req.commands {
        for c in &cmds {
            match c {
                Command::Create { id, net_index, .. } => {
                    s.held.insert(
                        id.clone(),
                        ObservedSandbox {
                            id: id.clone(),
                            tier: Tier::Hot,
                            serving: true,
                            ip: Some("10.86.0.2".into()),
                            net_index: *net_index,
                            egress: None,
                        },
                    );
                }
                Command::Destroy { id, .. } => {
                    s.held.remove(id);
                }
                Command::SetTier { id, tier } => {
                    if let Some(o) = s.held.get_mut(id) {
                        o.tier = *tier;
                    }
                }
                _ => {}
            }
        }
        s.batches.push(cmds);
    }
    Ok(Json(SyncResponse {
        report: PollRequest {
            // Deliberately wrong: the master must decide which node this is
            // from where it connected, not from what the body claims.
            node_id: "someone-else".into(),
            observed: s.held.values().cloned().collect(),
            free_memory_mib: 4096,
            pressure_some_avg10: 0.0,
            hold_ms: None,
            failures: vec![],
            deferred: vec![],
            traffic: vec![],
            evicted: vec![],
            features: vec![],
            tags: vec![],
            host: None,
            fingerprint: None,
            templates: None,
            pulls: vec![],
        },
        results: vec![],
        retry_in_ms: None,
    }))
}

/// A listening "node" on loopback, with a real identity from `dir`.
async fn fake_node(dir: &std::path::Path) -> (SocketAddr, String, Arc<FakeNode>) {
    let id = AgentIdentity::load_or_create(dir).unwrap();
    let node = Arc::new(FakeNode {
        secret: id.secret.clone(),
        state: Mutex::new(Fake::default()),
    });
    let app = Router::new()
        .route("/v1/agent/hello", get(fake_hello))
        .route("/v1/agent/sync", post(fake_sync))
        .with_state(node.clone());
    let tls = sandstorm_core::tls::plain_server_config(&id.cert_pem, &id.key_pem).unwrap();
    let std = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    std.set_nonblocking(true).unwrap();
    let addr = std.local_addr().unwrap();
    tokio::spawn(async move {
        axum_server::from_tcp_rustls(
            std,
            axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(tls)),
        )
        .unwrap()
        .serve(app.into_make_service())
        .await
        .ok();
    });
    (addr, id.pairing_code(), node)
}

async fn master() -> (String, TempDir) {
    let td = TempDir::new().unwrap();
    let st = Arc::new(MasterState::new(
        Store::open(td.path().join("state.db")).unwrap(),
        Ca::open_or_create(td.path().join("pki")).unwrap(),
        sandstorm_core::master::MasterOptions {
            port_range: (20000, 20010),
            admit: sandstorm_core::master::Admission {
                min_free_mib: 0,
                max_psi_avg10: f64::MAX,
            },
            ..Default::default()
        },
    ));
    let l = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(l, router(st)).await.ok();
    });
    (format!("http://{addr}"), td)
}

async fn eventually<F: Fn() -> bool>(what: &str, f: F) {
    for _ in 0..100 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for: {what}");
}

#[tokio::test]
async fn a_master_takes_over_a_listening_node_and_drives_it() {
    let nd = TempDir::new().unwrap();
    let (addr, code, node) = fake_node(nd.path()).await;
    let (m, _td) = master().await;
    let c = reqwest::Client::new();

    // The whole of joining: an address and the code. Nothing runs on the node.
    let r = c
        .post(format!("{m}/v1/nodes"))
        .json(&serde_json::json!({ "address": addr.to_string(), "pairing_code": code }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "{}", r.text().await.unwrap());
    let added: serde_json::Value = r.json().await.unwrap();
    assert_eq!(
        added["id"], "fake-1",
        "the id comes from the node by default"
    );
    assert_eq!(added["pool"], "default");

    // The driver's first sync marks it connected, as a first poll would.
    let mut connected = false;
    for _ in 0..100 {
        let ns: serde_json::Value = c
            .get(format!("{m}/v1/nodes"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if ns[0]["connected"] == true {
            assert_eq!(ns[0]["free_mib"], 4096, "and records what the node reports");
            connected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(connected, "the node never showed as connected");

    // What its fingerprint is made of, from the hello, reaches the console.
    let now: serde_json::Value = c
        .get(format!("{m}/v1/console/now"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(now["nodes"][0]["fingerprint"], "fp-fake");
    assert_eq!(now["nodes"][0]["host"]["kernel"], "6.1.0-fake");
    assert_eq!(now["nodes"][0]["host"]["cpu_flags"], "fpu sse2");

    // Placing a sandbox pushes a Create to the node -- the master initiates.
    c.post(format!("{m}/v1/templates"))
        .json(&serde_json::json!({
            "name": "py", "rootfs": "rootfs",
            "spec": { "args": ["/usr/bin/python3"], "memory_max_mib": 128 }
        }))
        .send()
        .await
        .unwrap();
    let made: serde_json::Value = c
        .post(format!("{m}/v1/sandboxes"))
        .json(&serde_json::json!({ "template": "py" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let sb = made["id"].as_str().unwrap().to_string();
    assert_eq!(made["node_id"], "fake-1");

    let n2 = node.clone();
    let sb2 = sb.clone();
    eventually("the node to be told to create the sandbox", move || {
        n2.state.lock().held.contains_key(&sb2)
    })
    .await;

    // And what the node reports flows back: the address it gave the sandbox.
    let mut ip = serde_json::Value::Null;
    for _ in 0..100 {
        let got: serde_json::Value = c
            .get(format!("{m}/v1/sandboxes/{sb}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        ip = got["ip"].clone();
        if ip == "10.86.0.2" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(ip, "10.86.0.2");

    // Deleting it pushes a Destroy.
    c.delete(format!("{m}/v1/sandboxes/{sb}"))
        .send()
        .await
        .unwrap();
    let n3 = node.clone();
    eventually("the node to be told to destroy it", move || {
        n3.state.lock().held.is_empty()
    })
    .await;
}

/// Converged and idle, the driver waits instead of re-sending -- otherwise
/// every listening node would cost the master a request loop.
#[tokio::test]
async fn an_idle_node_is_not_spun() {
    let nd = TempDir::new().unwrap();
    let (addr, code, node) = fake_node(nd.path()).await;
    let (m, _td) = master().await;
    reqwest::Client::new()
        .post(format!("{m}/v1/nodes"))
        .json(&serde_json::json!({ "address": addr.to_string(), "pairing_code": code }))
        .send()
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let syncs = node.state.lock().syncs;
    assert!(
        (1..=2).contains(&syncs),
        "{syncs} syncs in 1.5 s with nothing to do; expected the first and then a wait"
    );
}

#[tokio::test]
async fn a_code_for_another_certificate_is_refused() {
    let nd = TempDir::new().unwrap();
    let (addr, code, _node) = fake_node(nd.path()).await;
    let (m, _td) = master().await;
    let (_, secret) = sandstorm_core::tls::split_token(&code);
    let wrong = sandstorm_core::tls::join_token(&"ab".repeat(32), secret);

    let r = reqwest::Client::new()
        .post(format!("{m}/v1/nodes"))
        .json(&serde_json::json!({ "address": addr.to_string(), "pairing_code": wrong }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 502);
    let msg = r.text().await.unwrap();
    assert!(
        msg.contains("pairing code pins"),
        "should say the certificate did not match: {msg}"
    );
}

#[tokio::test]
async fn the_right_certificate_with_the_wrong_secret_is_refused() {
    let nd = TempDir::new().unwrap();
    let (addr, code, _node) = fake_node(nd.path()).await;
    let (m, _td) = master().await;
    let (pin, _) = sandstorm_core::tls::split_token(&code);
    let wrong = sandstorm_core::tls::join_token(pin.unwrap(), "0".repeat(32).as_str());

    let r = reqwest::Client::new()
        .post(format!("{m}/v1/nodes"))
        .json(&serde_json::json!({ "address": addr.to_string(), "pairing_code": wrong }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 502);
    assert!(r.text().await.unwrap().contains("refused the secret"));
}

#[tokio::test]
async fn nonsense_codes_and_addresses_are_rejected_before_dialing() {
    let (m, _td) = master().await;
    let c = reqwest::Client::new();
    for (address, code) in [
        ("10.0.0.1", "not-a-code"),
        ("10.0.0.1", "abc.def"),
        ("http://10.0.0.1/x", &format!("{}.{}", "a".repeat(64), "s")),
        ("fe80::1", &format!("{}.{}", "a".repeat(64), "s")),
    ] {
        let r = c
            .post(format!("{m}/v1/nodes"))
            .json(&serde_json::json!({ "address": address, "pairing_code": code }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "{address} / {code}");
    }
}

/// Revoking a listening node stops the master driving it -- nothing new
/// reaches it -- and adding it again with its code resumes.
#[tokio::test]
async fn revoking_stops_the_driver_and_adding_again_resumes() {
    let nd = TempDir::new().unwrap();
    let (addr, code, node) = fake_node(nd.path()).await;
    let (m, _td) = master().await;
    let c = reqwest::Client::new();
    let add = || {
        c.post(format!("{m}/v1/nodes"))
            .json(&serde_json::json!({ "address": addr.to_string(), "pairing_code": code }))
            .send()
    };
    assert_eq!(add().await.unwrap().status(), 201);
    c.post(format!("{m}/v1/templates"))
        .json(&serde_json::json!({
            "name": "py", "rootfs": "rootfs",
            "spec": { "args": ["/usr/bin/python3"], "memory_max_mib": 128 }
        }))
        .send()
        .await
        .unwrap();
    // Let the first sync land so the node counts as connected and placeable.
    tokio::time::sleep(Duration::from_millis(400)).await;

    assert_eq!(
        c.post(format!("{m}/v1/node/revoke/fake-1"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let before = node.state.lock().syncs;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        node.state.lock().syncs,
        before,
        "a revoked node must not be contacted again"
    );

    assert_eq!(
        add().await.unwrap().status(),
        201,
        "re-adding with the code is allowed"
    );
    let n2 = node.clone();
    eventually("the master to resume syncing", move || {
        n2.state.lock().syncs > before
    })
    .await;
}
