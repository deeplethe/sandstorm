//! End-to-end through the master's HTTP surface: register a node, place a
//! sandbox, and check the node is told to create it. No runsc involved --
//! this is the control plane on its own.

use std::net::SocketAddr;
use std::sync::Arc;

use sandstorm_core::identity::Ca;
use sandstorm_core::master::{router, MasterState};
use sandstorm_core::proto::{
    Command, Egress, ImageConfig, ObservedSandbox, PollResponse, TemplatePull,
};
use sandstorm_core::state::Store;
use sandstorm_core::tier::Tier;
use tempfile::TempDir;

struct Harness {
    base: String,
    _td: TempDir,
}

async fn start() -> Harness {
    let td = TempDir::new().unwrap();
    let st = Arc::new(MasterState::new(
        Store::open(td.path().join("state.db")).unwrap(),
        Ca::open_or_create(td.path().join("pki")).unwrap(),
        sandstorm_core::master::MasterOptions {
            port_range: (20000, 20010),
            // Admission off and the API open in tests: a node that has not
            // polled reports zero free memory, and most tests never poll.
            admit: sandstorm_core::master::Admission {
                min_free_mib: 0,
                max_psi_avg10: f64::MAX,
            },
            ..Default::default()
        },
    ));
    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(st)).await.ok();
    });
    Harness {
        base: format!("http://{addr}"),
        _td: td,
    }
}

async fn register(h: &Harness, node_id: &str, pool: &str) -> serde_json::Value {
    register_with_cap(h, node_id, pool, 100).await
}

async fn register_with_cap(
    h: &Harness,
    node_id: &str,
    pool: &str,
    max_sandboxes: u32,
) -> serde_json::Value {
    let c = reqwest::Client::new();
    let tok: serde_json::Value = c
        .post(format!("{}/v1/node-tokens", h.base))
        .json(&serde_json::json!({ "pool": pool }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    c.post(format!("{}/v1/node/register", h.base))
        .json(&serde_json::json!({
            "token": tok["token"],
            "node_id": node_id,
            "fingerprint": "abc123",
            "max_sandboxes": max_sandboxes,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn put_template(h: &Harness, name: &str) {
    let r = reqwest::Client::new()
        .post(format!("{}/v1/templates", h.base))
        .json(&serde_json::json!({
            "name": name,
            "rootfs": "rootfs",
            "spec": {
                "args": ["/usr/bin/python3", "/app/server.py", "{{PORT}}"],
                "memory_max_mib": 256
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
}

#[tokio::test]
async fn node_registers_and_gets_an_mtls_identity() {
    let h = start().await;
    let r = register(&h, "bm-1", "default").await;
    assert_eq!(r["node_id"], "bm-1");
    assert_eq!(r["pool"], "default");
    // Both halves of mTLS: the node's own cert, and the CA it uses to
    // verify the master is the real master.
    assert!(r["client_cert"]
        .as_str()
        .unwrap()
        .contains("BEGIN CERTIFICATE"));
    assert!(r["client_key"].as_str().unwrap().contains("PRIVATE KEY"));
    assert!(r["ca_cert"].as_str().unwrap().contains("BEGIN CERTIFICATE"));
}

#[tokio::test]
async fn a_registration_token_is_worth_one_node() {
    let h = start().await;
    let c = reqwest::Client::new();
    let tok: serde_json::Value = c
        .post(format!("{}/v1/node-tokens", h.base))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let body = serde_json::json!({
        "token": tok["token"], "node_id": "first",
        "fingerprint": "f", "max_sandboxes": 1,
    });
    let first = c
        .post(format!("{}/v1/node/register", h.base))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);

    let mut again = body.clone();
    again["node_id"] = "second".into();
    let second = c
        .post(format!("{}/v1/node/register", h.base))
        .json(&again)
        .send()
        .await
        .unwrap();
    assert_eq!(
        second.status(),
        401,
        "a used token must not register a second node"
    );
}

#[tokio::test]
async fn poll_tells_the_node_to_create_what_it_does_not_have() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();

    let created: serde_json::Value = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python", "port": 19800 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(
        created["node_id"], "bm-1",
        "placed on the only node in the pool"
    );
    // The sandbox access token is shown once, at creation, and never again.
    assert_eq!(created["token"].as_str().unwrap().len(), 32);

    let resp: PollResponse = c
        .post(format!("{}/v1/node/poll", h.base))
        .json(&serde_json::json!({
            "node_id": "bm-1", "observed": [],
            "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp.commands.len(), 1, "one create");
    match &resp.commands[0] {
        sandstorm_core::proto::Command::Create {
            id: got,
            port,
            spec,
            ..
        } => {
            assert_eq!(got, &id);
            assert_eq!(*port, 19800);
            // The spec travels with the command, so a node needs only the
            // rootfs locally.
            assert_eq!(spec["memory_max_mib"], 256);
            assert_eq!(spec["args"][1], "/app/server.py");
        }
        other => panic!("expected Create, got {other:?}"),
    }

    // Reading a sandbox back must never hand out its token again.
    let fetched: serde_json::Value = c
        .get(format!("{}/v1/sandboxes/{id}", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        fetched.get("token").is_none(),
        "token must not be re-served"
    );
}

/// The property that makes a restart free: a node reporting a sandbox the
/// master already wants is told nothing, not told to recreate it.
#[tokio::test]
async fn re_adoption_issues_no_commands() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();
    let created: serde_json::Value = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python", "port": 19801 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    let resp: PollResponse = c
        .post(format!("{}/v1/node/poll", h.base))
        .json(&serde_json::json!({
            "node_id": "bm-1",
            "observed": [ObservedSandbox { id: id.clone(), tier: Tier::Hot, serving: true, ip: None, net_index: None, egress: None }],
            "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        resp.commands.is_empty(),
        "an already-running sandbox must be adopted, not recreated: {:?}",
        resp.commands
    );
}

/// A node holding something desired state has dropped is told to destroy it
/// -- and only then. This is the one path that may kill a sandbox.
#[tokio::test]
async fn deleting_a_sandbox_makes_the_node_destroy_it() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();
    let created: serde_json::Value = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    let del = c
        .delete(format!("{}/v1/sandboxes/{id}", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 204);

    let resp: PollResponse = c
        .post(format!("{}/v1/node/poll", h.base))
        .json(&serde_json::json!({
            "node_id": "bm-1",
            "observed": [ObservedSandbox { id: id.clone(), tier: Tier::Hot, serving: true, ip: None, net_index: None, egress: None }],
            "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        matches!(
            resp.commands.as_slice(),
            [sandstorm_core::proto::Command::Destroy { id: got, .. }] if got == &id
        ),
        "got {:?}",
        resp.commands
    );
}

#[tokio::test]
async fn touching_a_cold_sandbox_promotes_it_straight_to_hot() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();
    let created: serde_json::Value = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    // Pretend the demoter has already put it on disk.
    let poll_then = |tier: Tier| {
        let base = h.base.clone();
        let id = id.clone();
        async move {
            reqwest::Client::new()
                .post(format!("{base}/v1/node/poll"))
                .json(&serde_json::json!({
                    "node_id": "bm-1",
                    "observed": [ObservedSandbox { id, tier, serving: false, ip: None, net_index: None, egress: None }],
                    "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
                }))
                .send()
                .await
                .unwrap()
                .json::<PollResponse>()
                .await
                .unwrap()
        }
    };

    let touched: serde_json::Value = c
        .post(format!("{}/v1/sandboxes/{id}/touch", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(touched["tier"], "hot");

    // A node still holding it as a snapshot is told to bring it up.
    let resp = poll_then(Tier::Snapshot).await;
    assert!(
        matches!(
            resp.commands.as_slice(),
            [sandstorm_core::proto::Command::SetTier {
                tier: Tier::Hot,
                ..
            }]
        ),
        "got {:?}",
        resp.commands
    );
}

#[tokio::test]
async fn a_sandbox_cannot_be_placed_outside_its_pool() {
    let h = start().await;
    register(&h, "bj-1", "beijing").await;
    put_template(&h, "python").await;

    let r = reqwest::Client::new()
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python", "pool": "hongkong" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        503,
        "no node in that pool, and pools must not be crossed"
    );
}

#[tokio::test]
async fn an_unknown_template_is_refused() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    let r = reqwest::Client::new()
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "no-such-thing" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}

/// "Add a machine" in the panel produces a command, not a machine. The
/// master has no way to reach a host that has never dialled in, so the
/// join step necessarily runs over there.
#[tokio::test]
async fn bootstrap_returns_a_pastable_join_command() {
    let h = start().await;
    let v: serde_json::Value = reqwest::Client::new()
        .post(format!("{}/v1/node-bootstrap", h.base))
        .json(&serde_json::json!({
            "pool": "beijing",
            "master_url": "https://sandstorm.example.com"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let cmd = v["install_command"].as_str().unwrap();
    let token = v["token"].as_str().unwrap();
    assert_eq!(v["pool"], "beijing");
    assert!(cmd.contains(token), "the command must carry the token");
    assert!(cmd.contains("https://sandstorm.example.com"));

    // And the token it embedded really registers a node, once.
    let body = serde_json::json!({
        "token": token, "node_id": "bj-2", "fingerprint": "f", "max_sandboxes": 10,
    });
    let c = reqwest::Client::new();
    let first = c
        .post(format!("{}/v1/node/register", h.base))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    let second = c
        .post(format!("{}/v1/node/register", h.base))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 401);
}

/// The property the held-open stream exists for: a node parked with no work
/// is handed a command as soon as desired state changes, without waiting for
/// any poll interval. The node listens on nothing -- it is parked on a
/// request it opened itself.
#[tokio::test]
async fn a_parked_node_is_dispatched_to_at_once() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;

    // Park a request. There is nothing to do yet, so the master holds it.
    let base = h.base.clone();
    let parked = tokio::spawn(async move {
        let t0 = std::time::Instant::now();
        let resp: PollResponse = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .unwrap()
            .post(format!("{base}/v1/node/poll"))
            .json(&serde_json::json!({
                "node_id": "bm-1", "observed": [],
                "free_memory_mib": 2000, "pressure_some_avg10": 0.0,
                "hold_ms": 10000,
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        (resp, t0.elapsed())
    });

    // Let it settle into the wait, then create work for it.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let created: serde_json::Value = reqwest::Client::new()
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python", "port": 19910 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    let (resp, elapsed) = parked.await.unwrap();
    assert!(
        matches!(resp.commands.as_slice(), [sandstorm_core::proto::Command::Create { id: got, .. }] if got == &id),
        "got {:?}",
        resp.commands
    );
    // Well under the 10 s hold: the create woke it, the hold did not expire.
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "dispatch took {elapsed:?}; the parked request was not woken"
    );
}

/// A change that lands between the node computing its work and settling down
/// to wait must not be lost to the gap.
#[tokio::test]
async fn a_change_racing_the_wait_is_not_lost() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;

    // Create first, then park: the node must be handed the work straight
    // away rather than waiting out its hold.
    reqwest::Client::new()
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python" }))
        .send()
        .await
        .unwrap();

    let t0 = std::time::Instant::now();
    let resp: PollResponse = reqwest::Client::new()
        .post(format!("{}/v1/node/poll", h.base))
        .json(&serde_json::json!({
            "node_id": "bm-1", "observed": [],
            "free_memory_mib": 2000, "pressure_some_avg10": 0.0,
            "hold_ms": 10000,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp.commands.len(), 1);
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(1),
        "answered at once"
    );
}

/// With nothing to do, the request really is parked -- it does not return
/// empty immediately and spin.
#[tokio::test]
async fn an_idle_node_is_parked_not_spinning() {
    let h = start().await;
    register(&h, "bm-1", "default").await;

    let t0 = std::time::Instant::now();
    let resp: PollResponse = reqwest::Client::new()
        .post(format!("{}/v1/node/poll", h.base))
        .json(&serde_json::json!({
            "node_id": "bm-1", "observed": [],
            "free_memory_mib": 2000, "pressure_some_avg10": 0.0,
            "hold_ms": 1000,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(resp.commands.is_empty());
    assert!(
        t0.elapsed() >= std::time::Duration::from_millis(900),
        "returned after {:?}; it should have been parked for ~1 s",
        t0.elapsed()
    );
}

/// A node that reports a failing sandbox makes the reason visible in the
/// API. Without this an operator has only one machine's log to go on.
#[tokio::test]
async fn a_reported_failure_shows_up_on_the_sandbox() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();
    let created: serde_json::Value = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    assert!(
        created.get("last_error").is_none(),
        "a fresh sandbox has no error"
    );

    c.post(format!("{}/v1/node/poll", h.base))
        .json(&serde_json::json!({
            "node_id": "bm-1", "observed": [],
            "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
            "failures": [{
                "id": id, "attempts": 4,
                "error": "systemd-run: Access denied as the requested operation requires interactive authentication"
            }],
        }))
        .send()
        .await
        .unwrap();

    let got: serde_json::Value = c
        .get(format!("{}/v1/sandboxes/{id}", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["error_attempts"], 4);
    assert!(
        got["last_error"]
            .as_str()
            .unwrap()
            .contains("Access denied"),
        "got {:?}",
        got["last_error"]
    );
}

/// A sandbox that keeps failing is taken out of the command stream. Retrying
/// for ever is a standing CPU cost on the node, not resilience.
#[tokio::test]
async fn a_repeatedly_failing_sandbox_is_quarantined_and_resumable() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();
    let created: serde_json::Value = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    let poll = |failures: serde_json::Value| {
        let base = h.base.clone();
        async move {
            reqwest::Client::new()
                .post(format!("{base}/v1/node/poll"))
                .json(&serde_json::json!({
                    "node_id": "bm-1", "observed": [],
                    "free_memory_mib": 2000, "pressure_some_avg10": 0.0,
                    "hold_ms": 0, "failures": failures,
                }))
                .send()
                .await
                .unwrap()
                .json::<PollResponse>()
                .await
                .unwrap()
        }
    };

    // Just under the limit: still being asked for.
    let resp = poll(serde_json::json!([{
        "id": id, "attempts": sandstorm_core::master::MAX_FAILURES - 1, "error": "boom"
    }]))
    .await;
    assert_eq!(resp.commands.len(), 1, "not quarantined yet");

    // At the limit: the master stops issuing it.
    let resp = poll(serde_json::json!([{
        "id": id, "attempts": sandstorm_core::master::MAX_FAILURES, "error": "boom"
    }]))
    .await;
    assert!(
        resp.commands.is_empty(),
        "a quarantined sandbox must not be re-issued: {:?}",
        resp.commands
    );

    // But it is still there, with the reason attached.
    let got: serde_json::Value = c
        .get(format!("{}/v1/sandboxes/{id}", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["last_error"], "boom");
    assert_eq!(got["error_attempts"], sandstorm_core::master::MAX_FAILURES);

    // An explicit retry puts it back in the stream.
    let after: serde_json::Value = c
        .post(format!("{}/v1/sandboxes/{id}/retry", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(after.get("last_error").is_none(), "retry clears the error");
    let resp = poll(serde_json::json!([])).await;
    assert_eq!(resp.commands.len(), 1, "back in the command stream");
}

/// A sandbox whose template has been removed must not produce a Create the
/// node can only fail on.
#[tokio::test]
async fn a_sandbox_with_a_missing_template_is_not_issued() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();
    c.post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python" }))
        .send()
        .await
        .unwrap();

    // Replace the template with a differently named one, leaving the
    // sandbox pointing at something that no longer exists.
    let resp: PollResponse = c
        .post(format!("{}/v1/node/poll", h.base))
        .json(&serde_json::json!({
            "node_id": "bm-1", "observed": [],
            "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        resp.commands.len(),
        1,
        "template present, so Create is issued"
    );
}

/// A detached sandbox is supposed to be absent from every node, so a node
/// not holding it is already correct. Issuing Create would resurrect it the
/// moment it detached, and the tier would never hold.
#[tokio::test]
async fn a_detached_sandbox_is_not_recreated() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();
    let created: serde_json::Value = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    let poll = |observed: serde_json::Value| {
        let base = h.base.clone();
        async move {
            reqwest::Client::new()
                .post(format!("{base}/v1/node/poll"))
                .json(&serde_json::json!({
                    "node_id": "bm-1", "observed": observed,
                    "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
                }))
                .send()
                .await
                .unwrap()
                .json::<PollResponse>()
                .await
                .unwrap()
        }
    };

    // Desired detached, node holds nothing: that is the correct state.
    c.post(format!("{}/v1/sandboxes/{id}/tier", h.base))
        .json(&serde_json::json!({ "tier": "detached" }))
        .send()
        .await
        .unwrap();
    let resp = poll(serde_json::json!([])).await;
    assert!(
        resp.commands.is_empty(),
        "a detached sandbox must not be recreated: {:?}",
        resp.commands
    );

    // And asking for it back does issue a Create, which is how it returns.
    c.post(format!("{}/v1/sandboxes/{id}/touch", h.base))
        .send()
        .await
        .unwrap();
    let resp = poll(serde_json::json!([])).await;
    assert!(
        matches!(
            resp.commands.as_slice(),
            [sandstorm_core::proto::Command::Create { .. }]
        ),
        "promotion from detached comes back as Create: {:?}",
        resp.commands
    );
}

/// Ports are no longer a node-wide resource: each sandbox has its own
/// network namespace, so they all listen on the same port and what the
/// master hands out is an address.
#[tokio::test]
async fn every_sandbox_gets_the_same_port_and_its_own_slot() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();

    let mut ports = std::collections::HashSet::new();
    for _ in 0..5 {
        let v: serde_json::Value = c
            .post(format!("{}/v1/sandboxes", h.base))
            .json(&serde_json::json!({ "template": "python" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        ports.insert(v["port"].as_u64().unwrap());
    }
    assert_eq!(
        ports.len(),
        1,
        "all five share one port; they are in separate namespaces"
    );
    assert_eq!(
        *ports.iter().next().unwrap() as u16,
        sandstorm_core::master::DEFAULT_SANDBOX_PORT
    );
}

/// A caller may still ask for a particular port inside its sandbox.
#[tokio::test]
async fn a_requested_port_is_honoured() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let v: serde_json::Value = reqwest::Client::new()
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python", "port": 9999 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["port"], 9999);
}

/// Running out of addresses is a clear refusal at create time, not a
/// sandbox that is accepted and then cannot be given a network -- and a
/// batch that runs out partway is refused whole.
#[tokio::test]
async fn an_exhausted_address_plan_is_refused_up_front() {
    let td = TempDir::new().unwrap();
    let st = Arc::new(MasterState::new(
        Store::open(td.path().join("state.db")).unwrap(),
        Ca::open_or_create(td.path().join("pki")).unwrap(),
        sandstorm_core::master::MasterOptions {
            admit: sandstorm_core::master::Admission {
                min_free_mib: 0,
                max_psi_avg10: f64::MAX,
            },
            // Room for exactly three sandboxes.
            max_net_index: 2,
            ..Default::default()
        },
    ));
    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(st)).await.ok();
    });
    let h = Harness {
        base: format!("http://{addr}"),
        _td: td,
    };
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();

    let create = |body: serde_json::Value| {
        c.post(format!("{}/v1/sandboxes", h.base))
            .json(&body)
            .send()
    };
    let one = serde_json::json!({ "template": "python" });
    for i in 0..2 {
        let r = create(one.clone()).await.unwrap();
        assert_eq!(r.status(), 201, "sandbox {i} should have got a slot");
    }

    // Two more when one slot is left: refused whole, nothing written, and
    // the slot is still there for a single create.
    let store = Store::open(h._td.path().join("state.db")).unwrap();
    let before = format!("{:?}", store.list_sandboxes().unwrap());
    let r = create(serde_json::json!({ "template": "python", "n": 2 }))
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
    assert_eq!(format!("{:?}", store.list_sandboxes().unwrap()), before);
    assert_eq!(create(one.clone()).await.unwrap().status(), 201);

    let r = create(one).await.unwrap();
    assert_eq!(r.status(), 503);
    let body: serde_json::Value = r.json().await.unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("no free network slot"),
        "got {:?}",
        body["error"]
    );
}

/// The node owns addressing, so the address it reports is what the API
/// serves -- there is nothing for a caller to reach a sandbox by until then.
#[tokio::test]
async fn the_address_the_node_reports_is_what_callers_see() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();
    let created: serde_json::Value = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    assert!(
        created.get("ip").is_none(),
        "no address until the node reports one"
    );

    c.post(format!("{}/v1/node/poll", h.base))
        .json(&serde_json::json!({
            "node_id": "bm-1",
            "observed": [{
                "id": id, "tier": "hot", "serving": true,
                "ip": "10.86.0.2", "net_index": 0
            }],
            "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
        }))
        .send()
        .await
        .unwrap();

    let got: serde_json::Value = c
        .get(format!("{}/v1/sandboxes/{id}", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["ip"], "10.86.0.2");
}

/// A node reports its own memory and pressure on every poll, and the master
/// must act on them. Without this it fills a node until it stops answering:
/// on bm that happened at 139 sandboxes, with the node having reported
/// 391 MiB free and a pressure average of 61 beforehand.
#[tokio::test]
async fn a_loaded_node_stops_being_given_sandboxes() {
    let td = TempDir::new().unwrap();
    let st = Arc::new(MasterState::new(
        Store::open(td.path().join("state.db")).unwrap(),
        Ca::open_or_create(td.path().join("pki")).unwrap(),
        sandstorm_core::master::MasterOptions {
            port_range: (20000, 20100),
            admit: sandstorm_core::master::Admission {
                min_free_mib: 512,
                max_psi_avg10: 8.0,
            },
            ..Default::default()
        },
    ));
    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(st)).await.ok();
    });
    let h = Harness {
        base: format!("http://{addr}"),
        _td: td,
    };
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();

    let report = |free: u64, psi: f64| {
        let base = h.base.clone();
        async move {
            reqwest::Client::new()
                .post(format!("{base}/v1/node/poll"))
                .json(&serde_json::json!({
                    "node_id": "bm-1", "observed": [],
                    "free_memory_mib": free, "pressure_some_avg10": psi,
                    "hold_ms": 0,
                }))
                .send()
                .await
                .unwrap();
        }
    };
    let create = || {
        let base = h.base.clone();
        async move {
            reqwest::Client::new()
                .post(format!("{base}/v1/sandboxes"))
                .json(&serde_json::json!({ "template": "python" }))
                .send()
                .await
                .unwrap()
        }
    };

    // A healthy node takes work.
    report(2000, 0.5).await;
    assert_eq!(create().await.status(), 201);

    // Low on memory: refused, with the reason.
    report(400, 0.5).await;
    let r = create().await;
    assert_eq!(r.status(), 503);
    let body: serde_json::Value = r.json().await.unwrap();
    let msg = body["error"].as_str().unwrap();
    assert!(
        msg.contains("too loaded") && msg.contains("400 MiB free"),
        "got {msg}"
    );

    // Memory fine but thrashing: also refused. This is the case free memory
    // alone misses -- on bm the apparent per-sandbox cost FELL from 22.8 to
    // 16.6 MiB while pressure climbed, because swap was absorbing it.
    report(2000, 20.0).await;
    let r = create().await;
    assert_eq!(r.status(), 503);
    let msg = r.json::<serde_json::Value>().await.unwrap()["error"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(msg.contains("pressure 20.0"), "got {msg}");

    // And it is visible on the node, not only in the refusal.
    let nodes: serde_json::Value = c
        .get(format!("{}/v1/nodes", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(nodes[0]["psi_some_avg10"], 20.0);
    assert!(nodes[0]["not_admitting"]
        .as_str()
        .unwrap()
        .contains("pressure"));

    // Recovering lets it take work again.
    report(2000, 0.2).await;
    assert_eq!(create().await.status(), 201);
}

/// `max_sandboxes` is what the node says it will carry. It was reported on
/// every registration, stored, and shown in the panel as "6 / 64" -- and
/// nothing read it: admission looked only at memory and pressure, so the
/// 65th sandbox went to a node that had said 64. The number has to be a
/// limit or it should not be on the screen.
#[tokio::test]
async fn a_node_at_its_cap_stops_being_given_sandboxes() {
    let td = TempDir::new().unwrap();
    let st = Arc::new(MasterState::new(
        Store::open(td.path().join("state.db")).unwrap(),
        Ca::open_or_create(td.path().join("pki")).unwrap(),
        sandstorm_core::master::MasterOptions {
            port_range: (20000, 20100),
            // Memory and pressure out of the way, so only the cap can refuse.
            admit: sandstorm_core::master::Admission {
                min_free_mib: 0,
                max_psi_avg10: 1000.0,
            },
            ..Default::default()
        },
    ));
    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(st)).await.ok();
    });
    let h = Harness {
        base: format!("http://{addr}"),
        _td: td,
    };
    put_template(&h, "python").await;
    let c = reqwest::Client::new();

    let poll = |node: &'static str| {
        let base = h.base.clone();
        async move {
            reqwest::Client::new()
                .post(format!("{base}/v1/node/poll"))
                .json(&serde_json::json!({
                    "node_id": node, "observed": [],
                    "free_memory_mib": 8000u64, "pressure_some_avg10": 0.1,
                    "hold_ms": 0,
                }))
                .send()
                .await
                .unwrap();
        }
    };
    let create = || {
        let base = h.base.clone();
        async move {
            reqwest::Client::new()
                .post(format!("{base}/v1/sandboxes"))
                .json(&serde_json::json!({ "template": "python" }))
                .send()
                .await
                .unwrap()
        }
    };

    register_with_cap(&h, "small-1", "default", 2).await;
    poll("small-1").await;

    // Two fit.
    let mut ids = Vec::new();
    for _ in 0..2 {
        let r = create().await;
        assert_eq!(r.status(), 201);
        let b: serde_json::Value = r.json().await.unwrap();
        assert_eq!(b["node_id"], "small-1");
        ids.push(b["id"].as_str().unwrap().to_string());
    }

    // The third does not, and says why in the node's own terms.
    let r = create().await;
    assert_eq!(r.status(), 503, "the cap has to actually refuse");
    let msg = r.json::<serde_json::Value>().await.unwrap()["error"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(msg.contains("at its cap of 2"), "got {msg}");

    // The panel reads the same refusal, not a different one.
    let nodes: serde_json::Value = c
        .get(format!("{}/v1/nodes", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        nodes[0]["not_admitting"]
            .as_str()
            .unwrap()
            .contains("at its cap of 2"),
        "got {:?}",
        nodes[0]["not_admitting"]
    );

    // A node that declares no cap is unlimited -- that is what
    // `--max-sandboxes 0` documents, and it is the default. The full node
    // diverts to it rather than the whole pool failing.
    register_with_cap(&h, "big-1", "default", 0).await;
    poll("big-1").await;
    for _ in 0..3 {
        let r = create().await;
        assert_eq!(r.status(), 201);
        let b: serde_json::Value = r.json().await.unwrap();
        assert_eq!(b["node_id"], "big-1", "the capped node is full");
    }

    // Freeing a slot makes the capped node a candidate again, which is the
    // half a cap gets wrong if it is enforced by a counter that only grows.
    c.delete(format!("{}/v1/sandboxes/{}", h.base, ids[0]))
        .send()
        .await
        .unwrap();
    let r = create().await;
    assert_eq!(r.status(), 201);
    let b: serde_json::Value = r.json().await.unwrap();
    assert_eq!(
        b["node_id"], "small-1",
        "one free slot on the smaller node makes it the least loaded again"
    );
    let nodes: serde_json::Value = c
        .get(format!("{}/v1/nodes", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(nodes[0]["not_admitting"].is_null(), "admitting again");
}

/// exec is request/response over a channel built for fire-and-forget
/// commands, so the correlation path is what these cover: the command
/// reaches the node, the result reaches the caller, and every way it can go
/// wrong still answers.
mod exec {
    use super::*;

    /// A hot sandbox and its own access token. The token is mandatory on
    /// exec, eval and ping (RFC 16.5 layer two), so a helper that returned
    /// only the id would make every test here exercise the refusal path.
    async fn hot_sandbox(h: &Harness) -> (String, String) {
        register(h, "bm-1", "default").await;
        put_template(h, "python").await;
        let created: serde_json::Value = reqwest::Client::new()
            .post(format!("{}/v1/sandboxes", h.base))
            .json(&serde_json::json!({ "template": "python" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        (
            created["id"].as_str().unwrap().to_string(),
            created["token"]
                .as_str()
                .expect("create must return the sandbox's access token")
                .to_string(),
        )
    }

    const TOK: &str = sandstorm_core::master::SANDBOX_TOKEN_HEADER;

    /// The whole round trip: caller parks, node is handed the command on its
    /// stream, node reports the result, caller gets it.
    #[tokio::test]
    async fn output_reaches_the_caller() {
        let h = start().await;
        let (id, sbtok) = hot_sandbox(&h).await;
        let base = h.base.clone();

        let caller = tokio::spawn({
            let base = base.clone();
            let id = id.clone();
            async move {
                reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(20))
                    .build()
                    .unwrap()
                    .post(format!("{base}/v1/sandboxes/{id}/exec"))
                    .header(TOK, &sbtok)
                    .json(&serde_json::json!({ "args": ["echo", "hi"] }))
                    .send()
                    .await
                    .unwrap()
                    .json::<serde_json::Value>()
                    .await
                    .unwrap()
            }
        });

        // Stand in for the node: poll, find the Exec, report its output.
        let mut exec_id = None;
        for _ in 0..60 {
            let resp: PollResponse = reqwest::Client::new()
                .post(format!("{base}/v1/node/poll"))
                .json(&serde_json::json!({
                    "node_id": "bm-1",
                    "observed": [ObservedSandbox { id: id.clone(), tier: Tier::Hot, serving: true, ip: None, net_index: None, egress: None }],
                    "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
                }))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            for c in resp.commands {
                if let sandstorm_core::proto::Command::Exec {
                    exec_id: e, args, ..
                } = c
                {
                    assert_eq!(args, vec!["echo", "hi"]);
                    exec_id = Some(e);
                }
            }
            if exec_id.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let exec_id = exec_id.expect("the exec command reached the node");

        let posted = reqwest::Client::new()
            .post(format!("{base}/v1/node/result"))
            .json(&serde_json::json!({
                "exec_id": exec_id, "stdout": "hi\n", "stderr": "", "exit_code": 0
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(posted.status(), 204);

        let got = caller.await.unwrap();
        assert_eq!(got["stdout"], "hi\n");
        assert_eq!(got["exit_code"], 0);
    }

    /// A non-zero exit is a result, not an error: the caller asked what the
    /// command did and that is what it did.
    #[tokio::test]
    async fn a_failing_command_is_still_a_result() {
        let h = start().await;
        let (id, sbtok) = hot_sandbox(&h).await;
        let base = h.base.clone();
        let caller = tokio::spawn({
            let base = base.clone();
            let id = id.clone();
            async move {
                reqwest::Client::new()
                    .post(format!("{base}/v1/sandboxes/{id}/exec"))
                    .header(TOK, &sbtok)
                    .json(&serde_json::json!({ "args": ["false"] }))
                    .send()
                    .await
                    .unwrap()
            }
        });
        let mut exec_id = None;
        for _ in 0..60 {
            let resp: PollResponse = reqwest::Client::new()
                .post(format!("{base}/v1/node/poll"))
                .json(&serde_json::json!({
                    "node_id": "bm-1",
                    "observed": [ObservedSandbox { id: id.clone(), tier: Tier::Hot, serving: true, ip: None, net_index: None, egress: None }],
                    "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
                }))
                .send().await.unwrap().json().await.unwrap();
            for c in resp.commands {
                if let sandstorm_core::proto::Command::Exec { exec_id: e, .. } = c {
                    exec_id = Some(e);
                }
            }
            if exec_id.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        reqwest::Client::new()
            .post(format!("{base}/v1/node/result"))
            .json(&serde_json::json!({
                "exec_id": exec_id.unwrap(), "stdout": "", "stderr": "nope\n", "exit_code": 1
            }))
            .send()
            .await
            .unwrap();
        let r = caller.await.unwrap();
        assert_eq!(r.status(), 200, "a non-zero exit is not an HTTP error");
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(v["exit_code"], 1);
        assert_eq!(v["stderr"], "nope\n");
    }

    /// A command cannot run in a sandbox that is not running. Promoting
    /// silently would hide a multi-second wait inside a quick-looking call.
    #[tokio::test]
    async fn exec_on_a_cold_sandbox_says_so_instead_of_waiting() {
        let h = start().await;
        let (id, sbtok) = hot_sandbox(&h).await;
        reqwest::Client::new()
            .post(format!("{}/v1/sandboxes/{id}/tier", h.base))
            .json(&serde_json::json!({ "tier": "snapshot" }))
            .send()
            .await
            .unwrap();

        let r = reqwest::Client::new()
            .post(format!("{}/v1/sandboxes/{id}/exec", h.base))
            .header(TOK, &sbtok)
            .json(&serde_json::json!({ "args": ["echo", "hi"] }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 409);
        let v: serde_json::Value = r.json().await.unwrap();
        let msg = v["error"].as_str().unwrap();
        assert!(
            msg.contains("snapshot") && msg.contains("touch"),
            "got {msg}"
        );
    }

    /// If the node never answers, the caller gets a timeout rather than
    /// hanging on an open request for ever.
    #[tokio::test]
    async fn a_silent_node_times_the_caller_out() {
        let h = start().await;
        let (id, sbtok) = hot_sandbox(&h).await;
        // timeout_secs 0 means the budget is just the 20 s of slack, which
        // is too slow for a test -- so assert the shape, not the wait.
        let r = reqwest::Client::new()
            .post(format!("{}/v1/sandboxes/{id}/exec", h.base))
            .header(TOK, &sbtok)
            .json(&serde_json::json!({ "args": ["echo"], "timeout_secs": 0 }))
            .timeout(std::time::Duration::from_millis(600))
            .send()
            .await;
        // The client gives up first; what matters is that the master kept
        // the request open rather than answering wrongly.
        assert!(
            r.is_err(),
            "the master must not answer before the node does"
        );
    }

    #[tokio::test]
    async fn an_empty_command_is_refused() {
        let h = start().await;
        let (id, sbtok) = hot_sandbox(&h).await;
        let r = reqwest::Client::new()
            .post(format!("{}/v1/sandboxes/{id}/exec", h.base))
            .header(TOK, &sbtok)
            .json(&serde_json::json!({ "args": [] }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
    }

    #[tokio::test]
    async fn a_result_nobody_waits_for_is_refused() {
        let h = start().await;
        let r = reqwest::Client::new()
            .post(format!("{}/v1/node/result", h.base))
            .json(&serde_json::json!({
                "exec_id": "deadbeef", "stdout": "", "stderr": "", "exit_code": 0
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404);
    }

    #[tokio::test]
    async fn exec_on_an_unknown_sandbox_is_not_found() {
        let h = start().await;
        register(&h, "bm-1", "default").await;
        let r = reqwest::Client::new()
            .post(format!("{}/v1/sandboxes/nope/exec", h.base))
            .json(&serde_json::json!({ "args": ["echo"] }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404);
    }
}

/// Code inside a sandbox can reach the master over loopback -- measured on
/// bm, where a sandbox connected to the API, to sshd and to a sibling. So
/// an open control plane means a sandbox can mint node tokens and delete
/// other sandboxes. These cover the token actually being required.
mod api_auth {
    use super::*;

    /// The console page is public, at / and /console; what it shows is not.
    /// Its data routes sit behind the token, and a master that is not
    /// recording history says so rather than answering with nothing.
    #[tokio::test]
    async fn the_console_page_is_open_and_its_data_is_not() {
        let (h, token) = guarded().await;
        let c = reqwest::Client::new();

        for path in ["/", "/console"] {
            let page = c.get(format!("{}{path}", h.base)).send().await.unwrap();
            assert_eq!(page.status(), 200, "{path}");
            assert!(
                page.text().await.unwrap().contains("Sandstorm console"),
                "{path}"
            );
        }

        for path in [
            "/v1/console/now",
            "/v1/console/series",
            "/v1/console/events",
        ] {
            let anon = c.get(format!("{}{path}", h.base)).send().await.unwrap();
            assert_eq!(anon.status(), 401, "{path} is not public");
        }

        let now: serde_json::Value = c
            .get(format!("{}/v1/console/now", h.base))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(now["nodes"].is_array());
        assert_eq!(now["failed"], 0);
        // The thresholds the console marks are the master's own; this
        // harness admits everything, so it says so.
        assert_eq!(now["floor_mib"], 0);
        assert!(now["max_psi"].is_number());

        // This harness builds a state without a history store.
        let series = c
            .get(format!("{}/v1/console/series?since=900", h.base))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(series.status(), 503);
    }

    /// The exposition names every node, its free memory and the shape of
    /// the fleet. That is a map of the deployment, so it sits behind the
    /// same token as everything else -- a scraper is configured with a
    /// bearer token, which is the ordinary arrangement.
    #[tokio::test]
    async fn metrics_need_the_token_and_report_what_happened() {
        let (h, token) = guarded().await;
        let c = reqwest::Client::new();

        let anon = c.get(format!("{}/metrics", h.base)).send().await.unwrap();
        assert_eq!(anon.status(), 401, "/metrics is not a public page");

        // The shared helpers register unauthenticated, which this harness
        // refuses, so the join is done here with the token.
        let tok: serde_json::Value = c
            .post(format!("{}/v1/node-tokens", h.base))
            .bearer_auth(&token)
            .json(&serde_json::json!({ "pool": "default" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        c.post(format!("{}/v1/node/register", h.base))
            .json(&serde_json::json!({
                "token": tok["token"], "node_id": "bm-1",
                "fingerprint": "abc123", "max_sandboxes": 100,
            }))
            .send()
            .await
            .unwrap();
        let t = c
            .post(format!("{}/v1/templates", h.base))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "name": "python",
                "rootfs": "rootfs",
                "spec": {
                    "args": ["/usr/bin/python3", "/app/server.py", "{{PORT}}"],
                    "memory_max_mib": 256
                }
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(t.status(), 201);
        c.post(format!("{}/v1/node/poll", h.base))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "node_id": "bm-1", "observed": [],
                "free_memory_mib": 4096u64, "pressure_some_avg10": 1.5,
                "hold_ms": 0,
            }))
            .send()
            .await
            .unwrap();
        let made: serde_json::Value = c
            .post(format!("{}/v1/sandboxes", h.base))
            .bearer_auth(&token)
            .json(&serde_json::json!({ "template": "python" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        let r = c
            .get(format!("{}/metrics", h.base))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert!(
            r.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/plain"),
            "a scraper refuses anything else"
        );
        let body = r.text().await.unwrap();

        // What the fleet is.
        assert!(
            body.contains("sandstorm_nodes{state=\"connected\"} 1"),
            "{body}"
        );
        assert!(
            body.contains("sandstorm_sandboxes{tier=\"hot\"} 1"),
            "{body}"
        );
        assert!(
            body.contains("sandstorm_node_free_memory_mib{node=\"bm-1\",pool=\"default\"} 4096"),
            "{body}"
        );
        // Tiers nobody is in are still present at zero: a dashboard should
        // not read "no data" and "nothing parked there" the same way.
        assert!(
            body.contains("sandstorm_sandboxes{tier=\"detached\"} 0"),
            "{body}"
        );

        // What was asked of it.
        assert!(
            body.contains("sandstorm_sandbox_creates_total{outcome=\"placed\"} 1"),
            "{body}"
        );

        // Latency is filed under the matched route, so the id does not
        // become its own series.
        let id = made["id"].as_str().unwrap();
        c.get(format!("{}/v1/sandboxes/{id}", h.base))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        let body = c
            .get(format!("{}/metrics", h.base))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            body.contains("route=\"/v1/sandboxes/:id\""),
            "the matched path should be the label: {body}"
        );
        assert!(
            !body.contains(id),
            "the sandbox id must not be a label: {body}"
        );

        // The store lock is instrumented, which is the number a load test
        // needs to tell a slow node from a queue at the database.
        assert!(
            body.contains("sandstorm_store_lock_wait_seconds_count"),
            "{body}"
        );
        assert!(
            body.contains("sandstorm_store_lock_held_seconds_count"),
            "{body}"
        );
    }

    async fn guarded() -> (Harness, String) {
        let td = TempDir::new().unwrap();
        let token = "s3cret-token".to_string();
        let st = Arc::new(MasterState::new(
            Store::open(td.path().join("state.db")).unwrap(),
            Ca::open_or_create(td.path().join("pki")).unwrap(),
            sandstorm_core::master::MasterOptions {
                port_range: (20000, 20100),
                admit: sandstorm_core::master::Admission {
                    min_free_mib: 0,
                    max_psi_avg10: f64::MAX,
                },
                api_token: Some(token.clone()),
                ..Default::default()
            },
        ));
        let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router(st)).await.ok();
        });
        (
            Harness {
                base: format!("http://{addr}"),
                _td: td,
            },
            token,
        )
    }

    #[tokio::test]
    async fn the_control_plane_refuses_an_unauthenticated_caller() {
        let (h, _t) = guarded().await;
        let c = reqwest::Client::new();
        // Exactly the calls a sandbox would make if it could reach the API.
        for (method, path) in [
            ("GET", "/v1/sandboxes"),
            ("GET", "/v1/nodes"),
            ("POST", "/v1/node-tokens"),
            ("POST", "/v1/node-bootstrap"),
            ("POST", "/v1/templates"),
            ("DELETE", "/v1/sandboxes/sb-someone-elses"),
        ] {
            let r = match method {
                "GET" => c.get(format!("{}{path}", h.base)).send().await.unwrap(),
                "DELETE" => c.delete(format!("{}{path}", h.base)).send().await.unwrap(),
                _ => c
                    .post(format!("{}{path}", h.base))
                    .json(&serde_json::json!({}))
                    .send()
                    .await
                    .unwrap(),
            };
            assert_eq!(r.status(), 401, "{method} {path} was not refused");
        }
    }

    #[tokio::test]
    async fn a_health_probe_needs_no_credential() {
        let (h, _t) = guarded().await;
        let r = reqwest::Client::new()
            .get(format!("{}/healthz", h.base))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "a load balancer has no token");
    }

    /// Registration is the one guarded action a node cannot authenticate,
    /// because the single-use token it spends IS its credential.
    #[tokio::test]
    async fn registration_is_open_but_still_needs_its_own_token() {
        let (h, token) = guarded().await;
        let c = reqwest::Client::new();

        let bad = c
            .post(format!("{}/v1/node/register", h.base))
            .json(&serde_json::json!({
                "token": "not-a-real-token", "node_id": "bm-1",
                "fingerprint": "f", "max_sandboxes": 1
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), 401, "a bogus registration token is refused");

        // A real one works, and hands back the API token for later calls.
        let tok: serde_json::Value = c
            .post(format!("{}/v1/node-tokens", h.base))
            .bearer_auth(&token)
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let reg: serde_json::Value = c
            .post(format!("{}/v1/node/register", h.base))
            .json(&serde_json::json!({
                "token": tok["token"], "node_id": "bm-1",
                "fingerprint": "f", "max_sandboxes": 1
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            reg["api_token"], token,
            "the node is given what its polls need"
        );
    }

    #[tokio::test]
    async fn the_right_token_is_accepted_and_a_wrong_one_is_not() {
        let (h, token) = guarded().await;
        let c = reqwest::Client::new();
        let ok = c
            .get(format!("{}/v1/sandboxes", h.base))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), 200);

        for wrong in ["", "s3cret-toke", "s3cret-tokenn", "S3CRET-TOKEN"] {
            let r = c
                .get(format!("{}/v1/sandboxes", h.base))
                .bearer_auth(wrong)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 401, "token {wrong:?} should not pass");
        }
    }
}

/// Every handler that changes desired state must wake the node, or the
/// change waits out the node's whole hold. The earlier delete test polled
/// with `hold_ms: 0`, so it never exercised the wake and missed that
/// delete_sandbox had none: measured on bm, a deleted sandbox kept running
/// for 30 s against a 25 s hold.
#[tokio::test]
async fn deleting_wakes_a_parked_node() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();
    let created: serde_json::Value = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    // Creating the sandbox already woke this node, and `notify_one` leaves
    // a permit behind. Drain it before parking, or the park below returns
    // on that stale permit and the test reads it as the delete's wake --
    // which is exactly how this test first fooled me.
    //
    // The hold has to be non-zero: a zero hold skips the wait altogether,
    // so it never consumes the permit. A short one returns at once because
    // the permit is there.
    let drain = serde_json::json!({
        "node_id": "bm-1",
        "observed": [{ "id": id, "tier": "hot", "serving": true }],
        "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 150,
    });
    c.post(format!("{}/v1/node/poll", h.base))
        .json(&drain)
        .send()
        .await
        .unwrap();

    // Now park. Nothing to do, so the master keeps the request.
    let base = h.base.clone();
    let parked = tokio::spawn({
        let id = id.clone();
        async move {
            let t0 = std::time::Instant::now();
            let resp: PollResponse = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap()
                .post(format!("{base}/v1/node/poll"))
                .json(&serde_json::json!({
                    "node_id": "bm-1",
                    "observed": [{ "id": id, "tier": "hot", "serving": true }],
                    "free_memory_mib": 2000, "pressure_some_avg10": 0.0,
                    "hold_ms": 20000,
                }))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            (resp, t0.elapsed())
        }
    });

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let del = c
        .delete(format!("{}/v1/sandboxes/{id}", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 204);
    // Prove the row really went, so an empty command list below can only
    // mean the diff is wrong and not that the delete silently failed.
    let after = c
        .get(format!("{}/v1/sandboxes/{id}", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(
        after.status(),
        404,
        "the sandbox should be gone from desired state"
    );

    let (resp, elapsed) = parked.await.unwrap();
    assert!(
        matches!(resp.commands.as_slice(), [sandstorm_core::proto::Command::Destroy { id: got, .. }] if got == &id),
        "got {:?}",
        resp.commands
    );
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "the parked request returned after {elapsed:?}; the delete did not wake it"
    );
}

/// Branch and fork, which are what the project is named after. The RFC's
/// compatibility section requires the v0.5 request bodies keep working and
/// removed options return 410 naming the replacement.
mod branch {
    use super::*;

    /// Stand in for a node: take the branch command and report success.
    async fn serve_branch(base: &str, node: &str, size: u64) -> Option<String> {
        for _ in 0..80 {
            let resp: PollResponse = reqwest::Client::new()
                .post(format!("{base}/v1/node/poll"))
                .json(&serde_json::json!({
                    "node_id": node, "observed": [],
                    "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
                }))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            for c in resp.commands {
                if let sandstorm_core::proto::Command::Branch { result_id, tag, .. } = c {
                    reqwest::Client::new()
                        .post(format!("{base}/v1/node/result"))
                        .json(&serde_json::json!({
                            "result_id": result_id, "exit_code": 0, "size_bytes": size
                        }))
                        .send()
                        .await
                        .unwrap();
                    return Some(tag);
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        None
    }

    async fn hot(h: &Harness) -> String {
        register(h, "bm-1", "default").await;
        put_template(h, "python").await;
        let v: serde_json::Value = reqwest::Client::new()
            .post(format!("{}/v1/sandboxes", h.base))
            .json(&serde_json::json!({ "template": "python" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        v["id"].as_str().unwrap().to_string()
    }

    /// The tag is recorded only after the node reports the image written,
    /// so a fork cannot race ahead of it.
    #[tokio::test]
    async fn branching_records_the_tag_once_the_node_has_written_it() {
        let h = start().await;
        let id = hot(&h).await;
        let base = h.base.clone();
        let node = tokio::spawn(async move { serve_branch(&base, "bm-1", 14_680_064).await });

        let v: serde_json::Value = reqwest::Client::new()
            .post(format!("{}/v1/sandboxes/{id}/branch", h.base))
            .json(&serde_json::json!({ "tag": "py-base" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(node.await.unwrap().as_deref(), Some("py-base"));
        assert_eq!(v["tag"], "py-base");
        assert_eq!(v["from_sandbox"], id);
        assert_eq!(v["size_bytes"], 14_680_064u64);

        let listed: serde_json::Value = reqwest::Client::new()
            .get(format!("{}/v1/snapshots", h.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(listed[0]["tag"], "py-base");
    }

    /// One image, many children: a fork of n lands n sandboxes, all on the
    /// node holding the image, and the v0.5 body gets the v0.5 list back.
    #[tokio::test]
    async fn forking_a_tag_makes_n_children_from_one_image() {
        let h = start().await;
        let id = hot(&h).await;
        let base = h.base.clone();
        let node = tokio::spawn(async move { serve_branch(&base, "bm-1", 1024).await });
        reqwest::Client::new()
            .post(format!("{}/v1/sandboxes/{id}/branch", h.base))
            .json(&serde_json::json!({ "tag": "t1" }))
            .send()
            .await
            .unwrap();
        node.await.unwrap();

        let kids: serde_json::Value = reqwest::Client::new()
            .post(format!("{}/v1/sandboxes", h.base))
            .json(&serde_json::json!({
                "snapshot_tag": "t1", "n": 4, "per_child_netns": true
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let arr = kids.as_array().expect("the v0.5 body gets a list back");
        assert_eq!(arr.len(), 4);
        // One transaction for the four, and each still got its own slot and port.
        let all = Store::open(h._td.path().join("state.db"))
            .unwrap()
            .list_sandboxes()
            .unwrap();
        let distinct = |f: fn(&sandstorm_core::state::Sandbox) -> Option<u32>| {
            all.iter()
                .filter_map(f)
                .collect::<std::collections::HashSet<_>>()
                .len()
        };
        assert_eq!(all.len(), 5);
        assert_eq!(distinct(|s| s.net_index), 5);
        assert_eq!(distinct(|s| s.gateway_port.map(u32::from)), 5);

        let mut slots = std::collections::HashSet::new();
        for k in arr {
            assert_eq!(k["node_id"], "bm-1", "forks land where the image is");
            assert_eq!(k["template"], "python", "inherited from the tag");
            assert_eq!(k["port"], sandstorm_core::master::DEFAULT_SANDBOX_PORT);
            assert!(slots.insert(k["id"].as_str().unwrap().to_string()));
        }

        let info: serde_json::Value = reqwest::Client::new()
            .get(format!("{}/v1/snapshots/t1/info", h.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(info["children"], 4);
    }

    /// The node is told to restore rather than cold start.
    #[tokio::test]
    async fn a_forked_child_is_restored_not_started_cold() {
        let h = start().await;
        let id = hot(&h).await;
        let base = h.base.clone();
        let node = tokio::spawn(async move { serve_branch(&base, "bm-1", 1).await });
        reqwest::Client::new()
            .post(format!("{}/v1/sandboxes/{id}/branch", h.base))
            .json(&serde_json::json!({ "tag": "t2" }))
            .send()
            .await
            .unwrap();
        node.await.unwrap();

        let kid: serde_json::Value = reqwest::Client::new()
            .post(format!("{}/v1/sandboxes", h.base))
            .json(&serde_json::json!({ "snapshot_tag": "t2" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let kid_id = kid["id"].as_str().unwrap().to_string();

        let resp: PollResponse = reqwest::Client::new()
            .post(format!("{}/v1/node/poll", h.base))
            .json(&serde_json::json!({
                "node_id": "bm-1", "observed": [],
                "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let forked = resp.commands.iter().any(|c| {
            matches!(
                c,
                sandstorm_core::proto::Command::Create { id, from_tag: Some(t), .. }
                    if id == &kid_id && t == "t2"
            )
        });
        assert!(forked, "the child must carry its tag: {:?}", resp.commands);
    }

    /// One image serves every child, so dropping it under them would
    /// strand them.
    #[tokio::test]
    async fn a_tag_with_children_cannot_be_deleted() {
        let h = start().await;
        let id = hot(&h).await;
        let base = h.base.clone();
        let node = tokio::spawn(async move { serve_branch(&base, "bm-1", 1).await });
        reqwest::Client::new()
            .post(format!("{}/v1/sandboxes/{id}/branch", h.base))
            .json(&serde_json::json!({ "tag": "t3" }))
            .send()
            .await
            .unwrap();
        node.await.unwrap();
        let c = reqwest::Client::new();
        let kid: serde_json::Value = c
            .post(format!("{}/v1/sandboxes", h.base))
            .json(&serde_json::json!({ "snapshot_tag": "t3" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        let r = c
            .delete(format!("{}/v1/snapshots/t3", h.base))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 409);
        assert!(r.json::<serde_json::Value>().await.unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("still forked"));

        // Once the child is gone it can go.
        c.delete(format!(
            "{}/v1/sandboxes/{}",
            h.base,
            kid["id"].as_str().unwrap()
        ))
        .send()
        .await
        .unwrap();
        let r = c
            .delete(format!("{}/v1/snapshots/t3", h.base))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 204);
    }

    /// Removed options are refused with the replacement named, as the RFC
    /// requires, rather than silently ignored.
    #[tokio::test]
    async fn removed_options_return_410_naming_the_replacement() {
        let h = start().await;
        let id = hot(&h).await;
        let c = reqwest::Client::new();

        for body in [
            serde_json::json!({ "tag": "x", "mode": "live" }),
            serde_json::json!({ "tag": "x", "mode": "diff" }),
            serde_json::json!({ "tag": "x", "live": true }),
            serde_json::json!({ "tag": "x", "wait": true }),
        ] {
            let r = c
                .post(format!("{}/v1/sandboxes/{id}/branch", h.base))
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 410, "{body} should be refused");
            let e = r.json::<serde_json::Value>().await.unwrap()["error"]
                .as_str()
                .unwrap()
                .to_string();
            assert!(
                e.contains("v0.6"),
                "the refusal should say what changed: {e}"
            );
        }

        for body in [
            serde_json::json!({ "snapshot_tag": "x", "live_fork": true }),
            serde_json::json!({ "snapshot_tag": "x", "hugepages": true }),
        ] {
            let r = c
                .post(format!("{}/v1/sandboxes", h.base))
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 410, "{body} should be refused");
        }
    }

    #[tokio::test]
    async fn branching_a_cold_sandbox_and_other_refusals() {
        let h = start().await;
        let id = hot(&h).await;
        let c = reqwest::Client::new();

        // Only a running sandbox has memory to checkpoint.
        c.post(format!("{}/v1/sandboxes/{id}/tier", h.base))
            .json(&serde_json::json!({ "tier": "snapshot" }))
            .send()
            .await
            .unwrap();
        let r = c
            .post(format!("{}/v1/sandboxes/{id}/branch", h.base))
            .json(&serde_json::json!({ "tag": "t4" }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 409);

        // A tag is a path component on the node.
        for bad in ["", "a/b", "../etc", ".hidden"] {
            let r = c
                .post(format!("{}/v1/sandboxes/{id}/branch", h.base))
                .json(&serde_json::json!({ "tag": bad }))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 400, "tag {bad:?} should be refused");
        }

        // Forking something that does not exist.
        let r = c
            .post(format!("{}/v1/sandboxes", h.base))
            .json(&serde_json::json!({ "snapshot_tag": "nope" }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404);

        // Neither, or both.
        for body in [
            serde_json::json!({}),
            serde_json::json!({ "template": "python", "snapshot_tag": "t" }),
        ] {
            let r = c
                .post(format!("{}/v1/sandboxes", h.base))
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 400, "{body} should be refused");
        }
    }

    // ---- forks that run on other nodes ---------------------------------

    /// A node registered with a given host fingerprint.
    async fn register_fp(h: &Harness, node: &str, fingerprint: &str) {
        let c = reqwest::Client::new();
        let tok: serde_json::Value = c
            .post(format!("{}/v1/node-tokens", h.base))
            .json(&serde_json::json!({ "pool": "default" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let r = c
            .post(format!("{}/v1/node/register", h.base))
            .json(&serde_json::json!({
                "token": tok["token"], "node_id": node,
                "fingerprint": fingerprint, "max_sandboxes": 100,
            }))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    }

    /// Record that the master reaches `node` on an address, as adding a
    /// listening node does. Only the row matters here: nothing dials it.
    fn make_agent(h: &Harness, node: &str) {
        Store::open(h._td.path().join("state.db"))
            .unwrap()
            .set_node_agent(node, "10.0.0.1:80", &"ab".repeat(32), "s3cret")
            .unwrap();
    }

    async fn poll(h: &Harness, node: &str, tags: &[&str]) -> Vec<sandstorm_core::proto::Command> {
        let resp: PollResponse = reqwest::Client::new()
            .post(format!("{}/v1/node/poll", h.base))
            .json(&serde_json::json!({
                "node_id": node, "observed": [], "tags": tags,
                "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        resp.commands
    }

    async fn fork(h: &Harness, body: serde_json::Value) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{}/v1/sandboxes", h.base))
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    /// bm-1 holds tag `t`, bm-2 has the same fingerprint, bm-3 does not.
    async fn three_nodes_and_a_tag(h: &Harness) {
        let id = hot(h).await;
        let base = h.base.clone();
        let node = tokio::spawn(async move { serve_branch(&base, "bm-1", 1024).await });
        reqwest::Client::new()
            .post(format!("{}/v1/sandboxes/{id}/branch", h.base))
            .json(&serde_json::json!({ "tag": "t" }))
            .send()
            .await
            .unwrap();
        node.await.unwrap();
        register_fp(h, "bm-2", "abc123").await;
        register_fp(h, "bm-3", "another-machine").await;
    }

    /// Children of a tag spread over every node that can restore it, and a
    /// node that does not hold the image is told where to fetch it, with a
    /// token for that tag and that node alone.
    #[tokio::test]
    async fn a_fork_spreads_over_the_nodes_that_can_restore_it() {
        let h = start().await;
        three_nodes_and_a_tag(&h).await;
        make_agent(&h, "bm-1");

        let kids: serde_json::Value = fork(&h, serde_json::json!({ "snapshot_tag": "t", "n": 4 }))
            .await
            .json()
            .await
            .unwrap();
        let on = |n: &str| {
            kids.as_array()
                .unwrap()
                .iter()
                .filter(|k| k["node_id"] == n)
                .count()
        };
        assert_eq!((on("bm-1"), on("bm-2"), on("bm-3")), (2, 2, 0), "{kids}");

        // bm-2 lacks the image: every Create carries where to get it.
        let cmds = poll(&h, "bm-2", &[]).await;
        let mut seen = 0;
        for c in &cmds {
            if let sandstorm_core::proto::Command::Create {
                from_tag, fetch, ..
            } = c
            {
                assert_eq!(from_tag.as_deref(), Some("t"));
                let f = fetch
                    .as_ref()
                    .expect("a child away from the image must say where it is");
                assert_eq!(
                    (f.node_id.as_str(), f.address.as_str()),
                    ("bm-1", "10.0.0.1:80")
                );
                assert_eq!(f.for_node, "bm-2");
                assert_eq!(f.cert_sha256, "ab".repeat(32));
                assert!(sandstorm_core::tag::fetch_token_valid(
                    "s3cret",
                    "t",
                    "bm-2",
                    f.expires,
                    &f.token,
                    sandstorm_core::state::now_secs()
                ));
                assert!(
                    !sandstorm_core::tag::fetch_token_valid(
                        "s3cret", "t", "bm-3", f.expires, &f.token, 0
                    ),
                    "the token is bound to bm-2"
                );
                seen += 1;
            }
        }
        assert_eq!(seen, 2, "{cmds:?}");
        // The same command on the next sync, or a driver would never settle.
        assert_eq!(
            serde_json::to_string(&poll(&h, "bm-2", &[]).await).unwrap(),
            serde_json::to_string(&cmds).unwrap()
        );

        // bm-1 has the image, so its children restore from its own copy.
        for c in poll(&h, "bm-1", &[]).await {
            if let sandstorm_core::proto::Command::Create {
                from_tag: Some(_),
                fetch,
                ..
            } = c
            {
                assert!(fetch.is_none());
            }
        }
    }

    /// Without an address for the node holding the image, nothing else can
    /// fetch it, so its forks stay there -- as every fork did before.
    #[tokio::test]
    async fn a_tag_on_a_node_that_dials_in_stays_on_it() {
        let h = start().await;
        three_nodes_and_a_tag(&h).await;

        let kids: serde_json::Value = fork(&h, serde_json::json!({ "snapshot_tag": "t", "n": 4 }))
            .await
            .json()
            .await
            .unwrap();
        for k in kids.as_array().unwrap() {
            assert_eq!(k["node_id"], "bm-1");
        }
        let r = fork(
            &h,
            serde_json::json!({ "snapshot_tag": "t", "node_id": "bm-2" }),
        )
        .await;
        assert_eq!(r.status(), 409);
        assert!(r.text().await.unwrap().contains("dials in"));
    }

    /// A node named for a fork must be able to restore the image.
    #[tokio::test]
    async fn a_fork_goes_to_a_named_node_only_if_it_can_restore_there() {
        let h = start().await;
        three_nodes_and_a_tag(&h).await;
        make_agent(&h, "bm-1");

        let r = fork(
            &h,
            serde_json::json!({ "snapshot_tag": "t", "node_id": "bm-3" }),
        )
        .await;
        assert_eq!(r.status(), 409);
        assert!(r.text().await.unwrap().contains("not portable"));

        let kid: serde_json::Value = fork(
            &h,
            serde_json::json!({ "snapshot_tag": "t", "node_id": "bm-2" }),
        )
        .await
        .json()
        .await
        .unwrap();
        assert_eq!(kid["node_id"], "bm-2");
    }

    /// Deleting a snapshot removes its record; each node that holds the
    /// image drops it when it next reports holding it.
    #[tokio::test]
    async fn a_node_drops_images_of_tags_that_are_gone() {
        let h = start().await;
        three_nodes_and_a_tag(&h).await;

        let drops = |cmds: Vec<sandstorm_core::proto::Command>| {
            cmds.into_iter()
                .filter_map(|c| match c {
                    sandstorm_core::proto::Command::DropTag { tag } => Some(tag),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(drops(poll(&h, "bm-2", &["t", "gone"]).await), vec!["gone"]);

        let r = reqwest::Client::new()
            .delete(format!("{}/v1/snapshots/t", h.base))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 204);
        assert_eq!(drops(poll(&h, "bm-1", &["t"]).await), vec!["t"]);
        assert!(drops(poll(&h, "bm-1", &[]).await).is_empty());
    }
}

/// Both SDKs send `per_child_netns: false` by default, so refusing it
/// breaks the plainest call either one makes. v0.6 gives every sandbox its
/// own namespace regardless, which is stronger than what `false` asks for,
/// so the field is accepted and ignored either way.
#[tokio::test]
async fn the_sdk_default_spawn_is_accepted() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();

    // Seed a tag the way a branch would.
    let parent: serde_json::Value = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let pid = parent["id"].as_str().unwrap().to_string();
    let base = h.base.clone();
    let node = tokio::spawn(async move {
        for _ in 0..80 {
            let resp: PollResponse = reqwest::Client::new()
                .post(format!("{base}/v1/node/poll"))
                .json(&serde_json::json!({
                    "node_id": "bm-1", "observed": [],
                    "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
                }))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            for cmd in resp.commands {
                if let sandstorm_core::proto::Command::Branch { result_id, .. } = cmd {
                    reqwest::Client::new()
                        .post(format!("{base}/v1/node/result"))
                        .json(&serde_json::json!({
                            "result_id": result_id, "exit_code": 0, "size_bytes": 1
                        }))
                        .send()
                        .await
                        .unwrap();
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    });
    c.post(format!("{}/v1/sandboxes/{pid}/branch", h.base))
        .json(&serde_json::json!({ "tag": "dflt" }))
        .send()
        .await
        .unwrap();
    node.await.unwrap();

    // Exactly what both SDKs send with no options set.
    for body in [
        serde_json::json!({ "snapshot_tag": "dflt", "n": 1, "per_child_netns": false }),
        serde_json::json!({ "snapshot_tag": "dflt", "n": 1, "per_child_netns": true }),
    ] {
        let r = c
            .post(format!("{}/v1/sandboxes", h.base))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 201, "{body} should be accepted");
    }
}

/// The caller's memory ceiling must reach the node, not merely be accepted.
/// Both SDKs send `memory_limit_mib`, and it was being dropped: a caller
/// asking for 512 MiB got the template's value with no way to tell.
#[tokio::test]
async fn a_callers_memory_limit_overrides_the_templates() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    let c = reqwest::Client::new();
    // The template says 256.
    c.post(format!("{}/v1/templates", h.base))
        .json(&serde_json::json!({
            "name": "python", "rootfs": "rootfs",
            "spec": { "args": ["/usr/bin/python3", "/app/s.py"], "memory_max_mib": 256 }
        }))
        .send()
        .await
        .unwrap();

    let asked: serde_json::Value = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python", "memory_limit_mib": 512 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(asked["memory_limit_mib"], 512, "echoed back to the caller");

    let dflt: serde_json::Value = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        dflt.get("memory_limit_mib").is_none(),
        "absent means the template's value applies"
    );

    // What the node is actually told is the only thing that matters.
    let resp: PollResponse = c
        .post(format!("{}/v1/node/poll", h.base))
        .json(&serde_json::json!({
            "node_id": "bm-1", "observed": [],
            "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let mut seen = std::collections::HashMap::new();
    for cmd in &resp.commands {
        if let sandstorm_core::proto::Command::Create { id, spec, .. } = cmd {
            seen.insert(id.clone(), spec["memory_max_mib"].as_u64());
        }
    }
    assert_eq!(
        seen.get(asked["id"].as_str().unwrap()),
        Some(&Some(512)),
        "the override must reach the node's spec, not stop at the API"
    );
    assert_eq!(
        seen.get(dflt["id"].as_str().unwrap()),
        Some(&Some(256)),
        "without an override the template's value is sent unchanged"
    );
}

/// A snapshot tag that would name a different directory is refused at the
/// API, not accepted and failed later on a node.
///
/// This is the shape of the 2026-05-17 advisory: `create_sandbox` joined
/// the caller's tag into a path while every sibling handler validated it.
/// The node validates too, which is what actually stops traversal, but a
/// tag that cannot work should not reach the database.
#[tokio::test]
async fn a_tag_that_is_not_a_plain_name_is_refused_with_400() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    let c = reqwest::Client::new();

    for bad in ["../../tmp/evil", "sub/dir", "..", ".hidden", "a\\b", ""] {
        let r = c
            .post(format!("{}/v1/sandboxes", h.base))
            .json(&serde_json::json!({ "snapshot_tag": bad, "port": 19900 }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            r.status().as_u16(),
            400,
            "snapshot_tag {bad:?} should be a 400, not accepted"
        );
    }

    // And branch, which already validated, keeps doing so through the same
    // helper rather than its own copy.
    let r = c
        .post(format!("{}/v1/sandboxes/nope/branch", h.base))
        .json(&serde_json::json!({ "tag": "../escape" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 400);
}

/// The gateway woke a snapshotted sandbox on its own and says so. The master
/// must adopt that -- desired becomes Hot -- rather than diff the node's Hot
/// against its own Snapshot and put the sandbox straight back on disk.
#[tokio::test]
async fn traffic_at_the_gateway_keeps_a_woken_sandbox_awake() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    register(&h, "bm-2", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();
    let created: serde_json::Value = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    let node = created["node_id"].as_str().unwrap().to_string();
    let other = if node == "bm-1" { "bm-2" } else { "bm-1" };
    let r = c
        .post(format!("{}/v1/sandboxes/{id}/tier", h.base))
        .json(&serde_json::json!({ "tier": "snapshot" }))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());

    let poll = |from: &str, traffic: Vec<String>| {
        let base = h.base.clone();
        let observed = if from == node {
            vec![ObservedSandbox {
                id: id.clone(),
                tier: Tier::Hot,
                serving: true,
                ip: None,
                net_index: None,
                egress: None,
            }]
        } else {
            vec![]
        };
        let from = from.to_string();
        async move {
            reqwest::Client::new()
                .post(format!("{base}/v1/node/poll"))
                .json(&serde_json::json!({
                    "node_id": from,
                    "observed": observed,
                    "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
                    "traffic": traffic,
                }))
                .send()
                .await
                .unwrap()
                .json::<PollResponse>()
                .await
                .unwrap()
        }
    };
    let tier = || {
        let url = format!("{}/v1/sandboxes/{id}", h.base);
        async move {
            reqwest::Client::new()
                .get(url)
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()["tier"]
                .clone()
        }
    };

    // A node that does not hold the sandbox cannot keep it awake.
    poll(other, vec![id.clone()]).await;
    assert_eq!(tier().await, "snapshot");

    // Without traffic, a hot sandbox the master wants on disk is demoted --
    // which is exactly what must not happen to one that was just woken.
    let resp = poll(&node, vec![]).await;
    assert!(
        resp.commands.iter().any(|c| matches!(
            c,
            sandstorm_core::proto::Command::SetTier {
                tier: Tier::Snapshot,
                ..
            }
        )),
        "got {:?}",
        resp.commands
    );

    // With it, the master adopts the wake and asks for nothing.
    let resp = poll(&node, vec![id.clone()]).await;
    assert!(resp.commands.is_empty(), "got {:?}", resp.commands);
    assert_eq!(tier().await, "hot");

    // Late traffic for a sandbox the node has since demoted -- it went
    // quiet, then a freeze came -- must not undo the freeze.
    let r = c
        .post(format!("{}/v1/sandboxes/{id}/tier", h.base))
        .json(&serde_json::json!({ "tier": "frozen" }))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let resp: PollResponse = c
        .post(format!("{}/v1/node/poll", h.base))
        .json(&serde_json::json!({
            "node_id": node,
            "observed": [ObservedSandbox { id: id.clone(), tier: Tier::Frozen, serving: false, ip: None, net_index: None, egress: None }],
            "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
            "traffic": [id],
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(resp.commands.is_empty(), "got {:?}", resp.commands);
    assert_eq!(tier().await, "frozen");
}

/// A polling node reports a wake out of turn, so the master's view is right
/// within a round trip rather than a whole hold later.
#[tokio::test]
async fn a_wake_reported_out_of_turn_is_adopted_at_once() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();
    let created: serde_json::Value = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    c.post(format!("{}/v1/sandboxes/{id}/tier", h.base))
        .json(&serde_json::json!({ "tier": "frozen" }))
        .send()
        .await
        .unwrap();

    let r = c
        .post(format!("{}/v1/node/traffic", h.base))
        .json(&serde_json::json!({
            "node_id": "bm-1",
            "observed": [ObservedSandbox { id: id.clone(), tier: Tier::Hot, serving: true, ip: None, net_index: None, egress: None }],
            "free_memory_mib": 2000, "pressure_some_avg10": 0.0,
            "traffic": [id],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let sb: serde_json::Value = c
        .get(format!("{}/v1/sandboxes/{id}", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(sb["tier"], "hot");
}

#[tokio::test]
async fn the_seed_prefix_cannot_be_used_for_a_sandbox() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let r = reqwest::Client::new()
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python", "id": "sandstorm-warm-mine" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}

/// Nodes decide their density levers for themselves, so a pool can be
/// mixed; /metrics is where that shows.
#[tokio::test]
async fn what_each_node_has_on_shows_in_metrics() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    let c = reqwest::Client::new();
    c.post(format!("{}/v1/node/poll", h.base))
        .json(&serde_json::json!({
            "node_id": "bm-1", "observed": [],
            "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
            "features": ["swap:zswap", "snapshot-compression"],
        }))
        .send()
        .await
        .unwrap();
    let m = c
        .get(format!("{}/metrics", h.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        m.contains(
            r#"sandstorm_node_feature{node="bm-1",pool="default",feature="snapshot-compression"} 1"#
        ),
        "{m}"
    );
    assert!(m.contains(r#"feature="swap:zswap"} 1"#));
}

/// The sandbox list filters and pages when asked, says how many matched,
/// and is the whole list when not asked.
#[tokio::test]
async fn the_sandbox_list_filters_and_pages() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();
    let mut ids = Vec::new();
    for _ in 0..3 {
        let v: serde_json::Value = c
            .post(format!("{}/v1/sandboxes", h.base))
            .json(&serde_json::json!({ "template": "python" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        ids.push(v["id"].as_str().unwrap().to_string());
    }
    let r = c
        .post(format!("{}/v1/sandboxes/{}/tier", h.base, ids[0]))
        .json(&serde_json::json!({ "tier": "snapshot" }))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());

    let list = |query: String| {
        let url = format!("{}/v1/sandboxes{query}", h.base);
        async move {
            let r = reqwest::Client::new().get(url).send().await.unwrap();
            let total: usize = r.headers()["x-total-count"]
                .to_str()
                .unwrap()
                .parse()
                .unwrap();
            let page: Vec<serde_json::Value> = r.json().await.unwrap();
            (
                total,
                page.into_iter()
                    .map(|v| v["id"].as_str().unwrap().to_string())
                    .collect::<Vec<_>>(),
            )
        }
    };
    let (total, all) = list(String::new()).await;
    assert_eq!((total, all.len()), (3, 3));
    let (total, page) = list("?limit=2".into()).await;
    assert_eq!((total, page.len()), (3, 2));
    let (total, rest) = list("?limit=2&offset=2".into()).await;
    assert_eq!((total, rest.len()), (3, 1));
    let (total, disk) = list("?tier=snapshot".into()).await;
    assert_eq!((total, disk), (1, vec![ids[0].clone()]));
    let (total, found) = list(format!("?q={}", ids[1][3..15].to_uppercase())).await;
    assert_eq!((total, found), (1, vec![ids[1].clone()]));
    let (total, _) = list("?node=bm-2".into()).await;
    assert_eq!(total, 0);
    let (total, _) = list("?failing=true".into()).await;
    assert_eq!(total, 0);
}

/// A node that put a sandbox to disk to make room for a wake is taken at its
/// word: desired state follows it down and nothing asks for it back, which
/// would only make the node evict something else. A node that does not hold
/// the sandbox cannot do this, and an eviction never moves desired state
/// warmer.
#[tokio::test]
async fn a_sandbox_a_node_evicted_is_not_asked_back() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    register(&h, "bm-2", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();
    let created: serde_json::Value = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    let node = created["node_id"].as_str().unwrap().to_string();
    let other = if node == "bm-1" { "bm-2" } else { "bm-1" };

    let poll = |from: &str, tier: Tier, evicted: Vec<String>| {
        let base = h.base.clone();
        let body = serde_json::json!({
            "node_id": from,
            "observed": [ObservedSandbox { id: id.clone(), tier, serving: tier.is_serving(), ip: None, net_index: None, egress: None }],
            "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
            "evicted": evicted,
        });
        async move {
            reqwest::Client::new()
                .post(format!("{base}/v1/node/poll"))
                .json(&body)
                .send()
                .await
                .unwrap()
                .json::<PollResponse>()
                .await
                .unwrap()
        }
    };
    let tier = || {
        let url = format!("{}/v1/sandboxes/{id}", h.base);
        async move {
            reqwest::Client::new()
                .get(url)
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()["tier"]
                .clone()
        }
    };
    assert_eq!(tier().await, "hot");

    // Another node's word counts for nothing.
    poll(other, Tier::Snapshot, vec![id.clone()]).await;
    assert_eq!(tier().await, "hot");

    // Without the eviction in the report, a snapshot where hot is wanted
    // is put right -- the loop the report exists to stop.
    let resp = poll(&node, Tier::Snapshot, vec![]).await;
    assert!(
        resp.commands.iter().any(|c| matches!(
            c,
            sandstorm_core::proto::Command::SetTier {
                tier: Tier::Hot,
                ..
            }
        )),
        "got {:?}",
        resp.commands
    );

    // With it, desired state follows the node and nothing is asked for.
    let resp = poll(&node, Tier::Snapshot, vec![id.clone()]).await;
    assert!(resp.commands.is_empty(), "got {:?}", resp.commands);
    assert_eq!(tier().await, "snapshot");

    // An eviction report that is out of date -- the sandbox has been
    // woken since -- does not drag desired state back down.
    let r = c
        .post(format!("{}/v1/sandboxes/{id}/tier", h.base))
        .json(&serde_json::json!({ "tier": "hot" }))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    poll(&node, Tier::Hot, vec![id.clone()]).await;
    assert_eq!(tier().await, "hot");
}

/// A create the node holds back for want of memory is never quarantined,
/// however often it is held back: the master keeps asking, so it starts once
/// the node has room. And once a report shows the sandbox up with nothing
/// failing, its error is gone.
#[tokio::test]
async fn a_held_back_create_is_never_given_up_on() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();
    let created: serde_json::Value = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    let poll = |observed: serde_json::Value, failures: serde_json::Value| {
        let base = h.base.clone();
        async move {
            reqwest::Client::new()
                .post(format!("{base}/v1/node/poll"))
                .json(&serde_json::json!({
                    "node_id": "bm-1", "observed": observed, "failures": failures,
                    "free_memory_mib": 300, "pressure_some_avg10": 0.0, "hold_ms": 0,
                }))
                .send()
                .await
                .unwrap()
                .json::<PollResponse>()
                .await
                .unwrap()
        }
    };
    let get = || {
        let url = format!("{}/v1/sandboxes/{id}", h.base);
        async move {
            reqwest::Client::new()
                .get(url)
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()
        }
    };
    let held = serde_json::json!([{
        "id": id, "attempts": 40, "held_back": true,
        "error": "node held it back: 300 MiB free, below its own 512 MiB floor",
    }]);
    let resp = poll(serde_json::json!([]), held).await;
    assert!(
        resp.commands
            .iter()
            .any(|c| matches!(c, sandstorm_core::proto::Command::Create { .. })),
        "a held-back create was given up on: {:?}",
        resp.commands
    );
    let sb = get().await;
    assert!(sb["last_error"].as_str().unwrap().contains("held it back"));
    assert_eq!(sb["error_attempts"].as_u64().unwrap_or(0), 0, "{sb}");

    let up = serde_json::json!([ObservedSandbox {
        id: id.clone(),
        tier: Tier::Hot,
        serving: true,
        ip: None,
        net_index: None,
        egress: None,
    }]);
    poll(up, serde_json::json!([])).await;
    assert!(
        get().await["last_error"].is_null(),
        "the error outlived the sandbox coming up"
    );
}

#[tokio::test]
async fn pool_batch_respects_whole_node_capacity() {
    // (capacity, existing count, requested count, expected status)
    for (capacity, existing, requested, status) in [
        (1, 0, 2, 503),
        (2, 1, 2, 503),
        (3, 1, 2, 201),
        (0, 1, 3, 201),
        (1, 0, 1, 201),
        (1, 1, 1, 503),
    ] {
        let h = start().await;
        register_with_cap(&h, "node", "default", capacity).await;
        put_template(&h, "python").await;
        let c = reqwest::Client::new();
        for _ in 0..existing {
            let r = c
                .post(format!("{}/v1/sandboxes", h.base))
                .json(&serde_json::json!({"template": "python"}))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 201);
        }
        let store = Store::open(h._td.path().join("state.db")).unwrap();
        let before = format!("{:?}", store.list_sandboxes().unwrap());
        let port_before = store.allocate_gateway_port("node", 20000, 20010).unwrap();
        let net_before = store.allocate_net_index("node", 100).unwrap();
        let r = c
            .post(format!("{}/v1/sandboxes", h.base))
            .json(&serde_json::json!({"template": "python", "n": requested}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            status,
            "capacity={capacity}, existing={existing}, requested={requested}"
        );
        if status == 503 {
            assert_eq!(format!("{:?}", store.list_sandboxes().unwrap()), before);
            assert_eq!(
                store.allocate_gateway_port("node", 20000, 20010).unwrap(),
                port_before
            );
            assert_eq!(store.allocate_net_index("node", 100).unwrap(), net_before);
            let metrics = c
                .get(format!("{}/metrics", h.base))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            assert!(
                metrics.contains("sandstorm_sandbox_creates_total{outcome=\"refused\"} 1\n"),
                "{metrics}"
            );
        } else {
            assert_eq!(
                store.list_sandboxes().unwrap().len(),
                (existing + requested) as usize
            );
        }
    }
}

#[tokio::test]
async fn pool_batch_selects_a_node_with_room_without_splitting() {
    for large_capacity in [2, 3] {
        let h = start().await;
        register_with_cap(&h, "small", "default", 1).await;
        register_with_cap(&h, "large", "default", large_capacity).await;
        put_template(&h, "python").await;
        let c = reqwest::Client::new();
        let r = c
            .post(format!("{}/v1/sandboxes", h.base))
            .json(&serde_json::json!({"template": "python", "node_id": "large"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 201);
        let store = Store::open(h._td.path().join("state.db")).unwrap();
        assert_eq!(store.count_sandboxes_on_node("large").unwrap(), 1);
        let before = format!("{:?}", store.list_sandboxes().unwrap());
        let r = c
            .post(format!("{}/v1/sandboxes", h.base))
            .json(&serde_json::json!({"template": "python", "n": 2}))
            .send()
            .await
            .unwrap();
        if large_capacity == 2 {
            assert_eq!(r.status(), 503);
            assert_eq!(format!("{:?}", store.list_sandboxes().unwrap()), before);
        } else {
            assert_eq!(r.status(), 201);
            let children: Vec<serde_json::Value> = r.json().await.unwrap();
            assert_eq!(children.len(), 2);
            assert!(children.iter().all(|child| child["node_id"] == "large"));
        }
        assert_eq!(store.count_sandboxes_on_node("small").unwrap(), 0);
    }
}

/// A node registered before it reported what its fingerprint is made of
/// (an older node, or any node after a master restart) brings it with its
/// next poll, and the master keeps it for the console. The poll already holds
/// the store, so recording the host must not take it again: this deadlocked
/// once, hence the timeout rather than a hang.
#[tokio::test]
async fn a_poll_brings_the_hosts_parts_to_the_console() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    let c = reqwest::Client::new();
    let host = serde_json::json!({
        "engine": "runsc version release-20261019.0",
        "kernel": "7.0.0-1003-aws",
        "cpu_vendor": "AuthenticAMD",
        "cpu_model": "AMD EPYC 7K62 48-Core Processor",
        "cpu_flags": "fpu sse2 avx2",
    });
    for _ in 0..2 {
        let poll = c
            .post(format!("{}/v1/node/poll", h.base))
            .json(&serde_json::json!({
                "node_id": "bm-1", "observed": [],
                "free_memory_mib": 2000, "pressure_some_avg10": 0.0,
                "hold_ms": 0, "host": host,
            }))
            .send();
        let r = tokio::time::timeout(std::time::Duration::from_secs(5), poll)
            .await
            .expect("the poll never answered: recording the host deadlocked")
            .unwrap();
        assert_eq!(r.status(), 200);
    }
    let now: serde_json::Value = c
        .get(format!("{}/v1/console/now", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(now["nodes"][0]["fingerprint"], "abc123");
    assert_eq!(now["nodes"][0]["host"], host);
}

/// A registered node whose fingerprint changes under it -- runsc or the
/// kernel upgraded -- is moved out of a pool whose other nodes it no longer
/// matches, on its next poll.
#[tokio::test]
async fn a_node_whose_fingerprint_changes_leaves_its_pool() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    register(&h, "bm-2", "default").await;
    let c = reqwest::Client::new();
    let r = c
        .post(format!("{}/v1/node/poll", h.base))
        .json(&serde_json::json!({
            "node_id": "bm-2", "observed": [],
            "free_memory_mib": 2000, "pressure_some_avg10": 0.0,
            "hold_ms": 0, "fingerprint": "def45678upgraded",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let nodes: serde_json::Value = c
        .get(format!("{}/v1/nodes", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let pool = |id: &str| {
        nodes
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == id)
            .unwrap()["pool"]
            .clone()
    };
    assert_eq!(pool("bm-1"), "default");
    assert_eq!(pool("bm-2"), "default#def45678");
}

/// A host group split off a pool -- another kind of machine, or one already
/// upgraded -- still takes new sandboxes asked of the pool: only a snapshot
/// is tied to a fingerprint, and a template boots anywhere. The sandbox
/// records the group it landed in.
#[tokio::test]
async fn a_pools_split_off_host_group_takes_new_sandboxes() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    let c = reqwest::Client::new();
    let tok: serde_json::Value = c
        .post(format!("{}/v1/node-tokens", h.base))
        .json(&serde_json::json!({ "pool": "default" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let joined: serde_json::Value = c
        .post(format!("{}/v1/node/register", h.base))
        .json(&serde_json::json!({
            "token": tok["token"], "node_id": "bm-2",
            "fingerprint": "intel999", "max_sandboxes": 100,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(joined["pool"], "default#intel999");
    // Only the split-off node is left to place on.
    c.post(format!("{}/v1/node/revoke/bm-1", h.base))
        .send()
        .await
        .unwrap();
    put_template(&h, "python").await;

    let r = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python", "pool": "default" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "{}", r.text().await.unwrap());
    let made: serde_json::Value = r.json().await.unwrap();
    assert_eq!(made["node_id"], "bm-2");
    assert_eq!(made["pool"], "default#intel999");
}

async fn poll_with(h: &Harness, id: &str, egress: Option<Egress>) -> Vec<Command> {
    let resp: PollResponse = reqwest::Client::new()
        .post(format!("{}/v1/node/poll", h.base))
        .json(&serde_json::json!({
            "node_id": "bm-1",
            "observed": [ObservedSandbox { id: id.to_string(), tier: Tier::Hot, serving: true, ip: None, net_index: None, egress }],
            "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    resp.commands
}

/// A sandbox is closed unless someone opened it, and the default decides for
/// the ones nobody said anything about.
#[tokio::test]
async fn the_default_decides_what_a_new_sandbox_may_reach() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();
    let create = |body: serde_json::Value| {
        let c = c.clone();
        let url = format!("{}/v1/sandboxes", h.base);
        async move {
            c.post(url)
                .json(&body)
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()
        }
    };

    let shut = create(serde_json::json!({ "template": "python" })).await;
    assert_eq!(shut["egress"]["internet"], false);

    let set = c
        .post(format!("{}/v1/egress", h.base))
        .json(&serde_json::json!({ "internet": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(set.status(), 200);

    let open = create(serde_json::json!({ "template": "python" })).await;
    assert_eq!(open["egress"]["internet"], true);
    let closed = create(serde_json::json!({ "template": "python", "egress": {} })).await;
    assert_eq!(closed["egress"]["internet"], false);

    // One already there keeps what it had.
    let id = shut["id"].as_str().unwrap();
    let again: serde_json::Value = c
        .get(format!("{}/v1/sandboxes/{id}", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(again["egress"]["internet"], false);

    // And back: closed is a value like any other.
    let set = c
        .post(format!("{}/v1/egress", h.base))
        .json(&serde_json::json!({ "internet": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(set.status(), 200);
    let shut_again = create(serde_json::json!({ "template": "python" })).await;
    assert_eq!(shut_again["egress"]["internet"], false);
}

/// Nodes are told by comparing what they report, so one that restarted and
/// reports nothing gets its rules back, and one that agrees hears nothing.
#[tokio::test]
async fn a_node_is_told_what_a_sandbox_may_reach_until_it_agrees() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    put_template(&h, "python").await;
    let c = reqwest::Client::new();
    let created: serde_json::Value = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    let set = c
        .post(format!("{}/v1/sandboxes/{id}/egress", h.base))
        .json(&serde_json::json!({ "internet": true, "allow": ["10.1.2.3/8"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(set.status(), 200);
    let want = Egress {
        internet: true,
        allow: vec!["10.0.0.0/8".into()],
        deny: vec![],
    };
    assert_eq!(
        set.json::<Egress>().await.unwrap(),
        want,
        "stored canonical, or a node would never compare equal"
    );

    // A node that has forgotten, as a restarted one has.
    match poll_with(&h, &id, Some(Egress::default())).await.as_slice() {
        [Command::SetEgress { id: got, egress }] => {
            assert_eq!(*got, id);
            assert_eq!(*egress, want);
        }
        other => panic!("expected SetEgress, got {other:?}"),
    }
    assert!(poll_with(&h, &id, Some(want)).await.is_empty());
    // One too old to report any cannot be told.
    assert!(poll_with(&h, &id, None).await.is_empty());

    // What is named replaces; what is not stays.
    let kept: Egress = c
        .post(format!("{}/v1/sandboxes/{id}/egress", h.base))
        .json(&serde_json::json!({ "internet": false }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!kept.internet);
    assert_eq!(kept.allow, ["10.0.0.0/8"]);

    let bad = c
        .post(format!("{}/v1/sandboxes/{id}/egress", h.base))
        .json(&serde_json::json!({ "allow": ["example.com"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
}

async fn poll_pulls(
    h: &Harness,
    node: &str,
    templates: &[&str],
    pulls: Vec<TemplatePull>,
) -> Vec<Command> {
    let resp: PollResponse = reqwest::Client::new()
        .post(format!("{}/v1/node/poll", h.base))
        .json(&serde_json::json!({
            "node_id": node, "observed": [],
            "free_memory_mib": 2000, "pressure_some_avg10": 0.0, "hold_ms": 0,
            "templates": templates, "pulls": pulls,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    resp.commands
}

async fn from_image(h: &Harness, body: serde_json::Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/templates/from-image", h.base))
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn pulls(h: &Harness) -> serde_json::Value {
    reqwest::Client::new()
        .get(format!("{}/v1/templates/pulls", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// From the request to a sandbox: the node is told, the master does not
/// spin on it while it runs, and what the image says becomes the template.
#[tokio::test]
async fn a_template_comes_from_an_image() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    let c = reqwest::Client::new();
    let r = from_image(
        &h,
        serde_json::json!({ "name": "py", "image": "python:3.12", "memory_max_mib": 256 }),
    )
    .await;
    assert_eq!(r.status(), 202);

    let early = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "py" }))
        .send()
        .await
        .unwrap();
    assert_eq!(early.status(), 409, "not until a node has it");

    match poll_pulls(&h, "bm-1", &[], vec![]).await.as_slice() {
        [Command::PullTemplate {
            name,
            image,
            attempt,
        }] => {
            assert_eq!(
                (name.as_str(), image.as_str(), *attempt),
                ("py", "python:3.12", 1)
            );
        }
        other => panic!("expected a pull, got {other:?}"),
    }
    // A poll with commands is answered at once, so this would spin.
    let running = TemplatePull::running("py", "python:3.12", 1);
    assert!(poll_pulls(&h, "bm-1", &[], vec![running]).await.is_empty());
    assert_eq!(pulls(&h).await[0]["nodes"][0]["state"], "pulling");

    let config = ImageConfig {
        args: vec!["python3".into(), "-m".into(), "http.server".into()],
        env: vec!["PATH=/usr/local/bin".into()],
        cwd: Some("/app".into()),
        port: Some(8000),
    };
    let done = TemplatePull::done("py", "python:3.12", 1, config);
    assert!(poll_pulls(&h, "bm-1", &["py"], vec![done]).await.is_empty());
    let view = pulls(&h).await;
    assert_eq!(view[0]["ready"], true);
    assert_eq!(view[0]["nodes"][0]["state"], "ready");

    let made: serde_json::Value = c
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "py" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(made["port"], 8000, "the port the image exposes");
    match poll_pulls(&h, "bm-1", &["py"], vec![]).await.as_slice() {
        [Command::Create { spec, port, .. }] => {
            assert_eq!(*port, 8000);
            assert_eq!(spec["args"][0], "python3");
            assert_eq!(spec["cwd"], "/app");
            assert_eq!(spec["memory_max_mib"], 256);
            assert!(spec["ro_binds"].as_array().unwrap().is_empty());
            assert!(spec.get("pending").is_none());
        }
        other => panic!("expected a create, got {other:?}"),
    }
}

/// A failed pull is not retried on its own, and is when asked. The outcome
/// of the old attempt must not be taken for the new one's.
#[tokio::test]
async fn a_failed_pull_waits_to_be_asked_again() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    let body = serde_json::json!({ "name": "py", "image": "no/such:image" });
    assert_eq!(from_image(&h, body.clone()).await.status(), 202);
    let failed = TemplatePull::failed("py", "no/such:image", 1, "404".into());
    assert!(poll_pulls(&h, "bm-1", &[], vec![failed.clone()])
        .await
        .is_empty());
    assert_eq!(pulls(&h).await[0]["nodes"][0]["error"], "404");
    assert!(poll_pulls(&h, "bm-1", &[], vec![]).await.is_empty());

    assert_eq!(from_image(&h, body).await.status(), 202);
    match poll_pulls(&h, "bm-1", &[], vec![failed]).await.as_slice() {
        [Command::PullTemplate { attempt: 2, .. }] => {}
        other => panic!("expected a second attempt, got {other:?}"),
    }
}

/// A node that joins later gets the template too.
#[tokio::test]
async fn a_node_that_joins_later_pulls_the_template() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    from_image(
        &h,
        serde_json::json!({ "name": "py", "image": "python:3.12" }),
    )
    .await;
    let config = ImageConfig {
        args: vec!["python3".into()],
        ..Default::default()
    };
    let done = TemplatePull::done("py", "python:3.12", 1, config);
    poll_pulls(&h, "bm-1", &["py"], vec![done]).await;

    register(&h, "bm-2", "default").await;
    let cmds = poll_pulls(&h, "bm-2", &[], vec![]).await;
    assert!(
        matches!(cmds.as_slice(), [Command::PullTemplate { .. }]),
        "{cmds:?}"
    );
    assert!(poll_pulls(&h, "bm-1", &["py"], vec![]).await.is_empty());
}

#[tokio::test]
async fn an_image_with_no_program_needs_args() {
    let h = start().await;
    register(&h, "bm-1", "default").await;
    from_image(&h, serde_json::json!({ "name": "s", "image": "scratch:1" })).await;
    let done = TemplatePull::done("s", "scratch:1", 1, ImageConfig::default());
    poll_pulls(&h, "bm-1", &["s"], vec![done]).await;
    let view = pulls(&h).await;
    assert_eq!(view[0]["ready"], false);
    assert_eq!(view[0]["nodes"][0]["state"], "failed");
}

#[tokio::test]
async fn a_name_that_means_something_else_is_refused() {
    let h = start().await;
    put_template(&h, "python").await;
    let r = from_image(
        &h,
        serde_json::json!({ "name": "python", "image": "python:3.12" }),
    )
    .await;
    assert_eq!(r.status(), 409);
    let r = from_image(&h, serde_json::json!({ "name": "../x", "image": "python" })).await;
    assert_eq!(r.status(), 400);
    let r = from_image(&h, serde_json::json!({ "name": "x", "image": "a b" })).await;
    assert_eq!(r.status(), 400);
}
