//! RFC 16.5 layer two, and the fingerprint as a pool boundary.
//!
//! Layer one (#329) answers "who may be a node". This is the other two
//! questions the RFC separates from it: "who may enter this sandbox", and
//! "which hosts may hold one pool's snapshots".

use std::net::SocketAddr;
use std::sync::Arc;

use sandstorm_core::identity::Ca;
use sandstorm_core::master::{router, MasterOptions, MasterState, SANDBOX_TOKEN_HEADER};
use sandstorm_core::state::{Node, Store};
use tempfile::TempDir;

struct Harness {
    base: String,
    st: Arc<MasterState>,
    _td: TempDir,
}

async fn start() -> Harness {
    let td = TempDir::new().unwrap();
    let st = Arc::new(MasterState::new(
        Store::open(td.path().join("state.db")).unwrap(),
        Ca::open_or_create(td.path().join("pki")).unwrap(),
        MasterOptions {
            port_range: (20000, 20050),
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
    let app = router(st.clone());
    tokio::spawn(async move { axum::serve(l, app).await.ok() });
    Harness {
        base: format!("http://{addr}"),
        st,
        _td: td,
    }
}

/// Put a node in the store directly, so a fingerprint can be chosen.
fn add_node(h: &Harness, id: &str, pool: &str, fingerprint: &str) {
    let store = h.st.store();
    store
        .upsert_node(&Node {
            id: id.into(),
            pool: pool.into(),
            fingerprint: fingerprint.into(),
            max_sandboxes: 100,
            last_seen_at: 0,
            connected: true,
            free_mib: 4096,
            psi_some_avg10: 0.0,
        })
        .unwrap();
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
    assert_eq!(r.status(), 201, "template setup");
}

/// Returns (id, token).
async fn create(h: &Harness) -> (String, String) {
    let v: serde_json::Value = reqwest::Client::new()
        .post(format!("{}/v1/sandboxes", h.base))
        .json(&serde_json::json!({ "template": "python" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    (
        v["id"].as_str().unwrap().to_string(),
        v["token"]
            .as_str()
            .expect("create returns a token")
            .to_string(),
    )
}

// ---- who may enter this sandbox -----------------------------------------

#[tokio::test]
async fn knowing_the_sandbox_id_is_not_enough() {
    let h = start().await;
    add_node(&h, "bm-1", "default", "fp-a");
    put_template(&h, "python").await;
    let (id, token) = create(&h).await;
    let c = reqwest::Client::new();

    // This is the defect RFC 16.5 names: in v0.5 the traffic token was
    // opt-in, so a caller who learned an id was in.
    for (header, what) in [
        (None, "no token at all"),
        (Some("00000000000000000000000000000000"), "a wrong token"),
    ] {
        let mut rq = c
            .post(format!("{}/v1/sandboxes/{id}/exec", h.base))
            .json(&serde_json::json!({ "args": ["true"] }));
        if let Some(t) = header {
            rq = rq.header(SANDBOX_TOKEN_HEADER, t);
        }
        assert_eq!(
            rq.send().await.unwrap().status().as_u16(),
            403,
            "{what} must be refused"
        );
    }

    // With the right token the request gets past this layer and on to
    // dispatch, where no node is standing by in this harness -- so it
    // parks. Only "no longer 403" belongs here; asserting the next layer's
    // status would be testing dispatch, and waiting out its timeout is what
    // made this test take 50 seconds.
    let parked = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(600))
        .build()
        .unwrap()
        .post(format!("{}/v1/sandboxes/{id}/exec", h.base))
        .header(SANDBOX_TOKEN_HEADER, &token)
        .json(&serde_json::json!({ "args": ["true"] }))
        .send()
        .await;
    match parked {
        Err(e) => assert!(e.is_timeout(), "expected it to park, got {e}"),
        Ok(r) => assert_ne!(r.status().as_u16(), 403, "the right token must pass"),
    }
}

#[tokio::test]
async fn eval_and_ping_demand_it_too() {
    let h = start().await;
    add_node(&h, "bm-1", "default", "fp-a");
    put_template(&h, "python").await;
    let (id, token) = create(&h).await;
    let c = reqwest::Client::new();

    // Every path that reaches into a sandbox, not just exec -- a check on
    // one of three doors is not a check.
    assert_eq!(
        c.post(format!("{}/v1/sandboxes/{id}/eval", h.base))
            .json(&serde_json::json!({ "code": "1" }))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        403,
        "eval without the token"
    );
    assert_eq!(
        c.get(format!("{}/v1/sandboxes/{id}/ping", h.base))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        403,
        "ping without the token"
    );
    // And that the token is what makes the difference, not the method.
    assert_ne!(
        c.get(format!("{}/v1/sandboxes/{id}/ping", h.base))
            .header(SANDBOX_TOKEN_HEADER, &token)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        403,
        "ping with the token"
    );
}

#[tokio::test]
async fn one_sandboxs_token_does_not_open_another() {
    let h = start().await;
    add_node(&h, "bm-1", "default", "fp-a");
    put_template(&h, "python").await;
    let (_a, token_a) = create(&h).await;
    let (b, _token_b) = create(&h).await;

    assert_eq!(
        reqwest::Client::new()
            .get(format!("{}/v1/sandboxes/{b}/ping", h.base))
            .header(SANDBOX_TOKEN_HEADER, &token_a)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        403,
        "a's token must not reach b"
    );
}

#[tokio::test]
async fn the_refusal_does_not_depend_on_the_sandbox_existing() {
    let h = start().await;
    add_node(&h, "bm-1", "default", "fp-a");
    // An unknown id is 404, a known one without a token is 403. Both are
    // refusals; what matters is that neither leaks the token.
    let r = reqwest::Client::new()
        .get(format!("{}/v1/sandboxes/sb-does-not-exist/ping", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 404);
    let body = r.text().await.unwrap();
    assert!(
        !body.contains("token") || body.contains("no sandbox"),
        "{body}"
    );
}

#[tokio::test]
async fn ids_and_tokens_are_both_128_bits() {
    let h = start().await;
    add_node(&h, "bm-1", "default", "fp-a");
    put_template(&h, "python").await;
    let (id, token) = create(&h).await;

    // `sb-` plus 32 hex characters. The old id was 12 hex = 48 bits, which
    // RFC 16.5 calls out as guessable.
    let hex = id.strip_prefix("sb-").expect("sb- prefix");
    assert_eq!(hex.len(), 32, "id {id} should carry 128 bits");
    assert!(hex.chars().all(|c| c.is_ascii_hexdigit()), "id {id}");
    assert_eq!(token.len(), 32, "token should carry 128 bits");

    // And two sandboxes do not collide or share.
    let (id2, token2) = create(&h).await;
    assert_ne!(id, id2);
    assert_ne!(token, token2);
}

// ---- which hosts may hold one pool's snapshots ---------------------------

#[tokio::test]
async fn a_mismatched_fingerprint_does_not_join_the_pool() {
    let h = start().await;
    add_node(&h, "bm-1", "default", "fp-matching");

    // Register a second node into the same pool with a different
    // fingerprint. It must not end up beside the first: a pool is a
    // migration domain, and runsc will not restore across differing hosts.
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
    let r: serde_json::Value = c
        .post(format!("{}/v1/node/register", h.base))
        .json(&serde_json::json!({
            "token": tok["token"], "node_id": "bm-2",
            "fingerprint": "fp-different", "max_sandboxes": 10,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let joined = r["pool"].as_str().unwrap();
    assert_ne!(joined, "default", "it must not join the mismatched pool");
    assert!(
        joined.starts_with("default#"),
        "the derived pool should say which pool it was asked for: {joined}"
    );
}

#[tokio::test]
async fn a_matching_fingerprint_joins_normally() {
    let h = start().await;
    add_node(&h, "bm-1", "default", "fp-same");

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
    let r: serde_json::Value = c
        .post(format!("{}/v1/node/register", h.base))
        .json(&serde_json::json!({
            "token": tok["token"], "node_id": "bm-2",
            "fingerprint": "fp-same", "max_sandboxes": 10,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["pool"].as_str().unwrap(), "default");
}

#[tokio::test]
async fn placement_spreads_across_a_pool() {
    let h = start().await;
    add_node(&h, "bm-1", "default", "fp-same");
    add_node(&h, "bm-2", "default", "fp-same");
    put_template(&h, "python").await;

    // Least-loaded placement: four sandboxes over two nodes should not all
    // land on one. This is the first test that exercises more than one node
    // at all -- until now the fleet half of master/node was untested.
    let mut on: std::collections::HashMap<String, usize> = Default::default();
    for _ in 0..4 {
        let v: serde_json::Value = reqwest::Client::new()
            .post(format!("{}/v1/sandboxes", h.base))
            .json(&serde_json::json!({ "template": "python" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        *on.entry(v["node_id"].as_str().unwrap().to_string())
            .or_default() += 1;
    }
    assert_eq!(on.len(), 2, "both nodes should get work: {on:?}");
    assert_eq!(on["bm-1"], 2);
    assert_eq!(on["bm-2"], 2);
}

/// A sandbox started from a template boots on any host, so one asked of
/// `default` may land in a host group split off it (`default#fp-b`): only a
/// snapshot is tied to a fingerprint, and forks are placed by the tag's.
/// Nothing moves a running sandbox's checkpoint between nodes -- a detached
/// one wakes where it was -- so the group it starts in is where it stays.
#[tokio::test]
async fn a_template_sandbox_spreads_over_a_pools_host_groups() {
    let h = start().await;
    add_node(&h, "bm-1", "default", "fp-a");
    add_node(&h, "bm-2", "default#fp-b", "fp-b");
    put_template(&h, "python").await;

    let mut on: std::collections::HashMap<String, (u32, String)> = Default::default();
    for _ in 0..4 {
        let v: serde_json::Value = reqwest::Client::new()
            .post(format!("{}/v1/sandboxes", h.base))
            .json(&serde_json::json!({ "template": "python", "pool": "default" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let e = on
            .entry(v["node_id"].as_str().unwrap().to_string())
            .or_default();
        e.0 += 1;
        e.1 = v["pool"].as_str().unwrap().to_string();
    }
    assert_eq!(on["bm-1"], (2, "default".into()), "{on:?}");
    assert_eq!(on["bm-2"], (2, "default#fp-b".into()), "{on:?}");
}
