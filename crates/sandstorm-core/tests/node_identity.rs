//! The node control channel's authentication.
//!
//! Two layers are tested separately because they fail differently:
//!
//! - `identity_*`: given a presented certificate, does the master resolve it
//!   to the right node and refuse the wrong ones. Run over plain HTTP with
//!   the certificate fingerprint injected, so the cases (unknown, revoked,
//!   mismatched) can be set up exactly.
//! - `transport_*`: does a real TLS handshake actually happen, with a real
//!   client certificate, and does the master read the identity off it. This
//!   is the one that would catch the state this code was written to fix --
//!   certificates issued, tested, and then never served.

use std::net::SocketAddr;
use std::sync::Arc;

use sandstorm_core::identity::Ca;
use sandstorm_core::master::{router, MasterOptions, MasterState};
use sandstorm_core::state::Store;
use sandstorm_core::tls::{self, PeerCert};
use tempfile::TempDir;

struct Harness {
    base: String,
    st: Arc<MasterState>,
    _td: TempDir,
}

fn state(td: &TempDir, require_certs: bool) -> Arc<MasterState> {
    Arc::new(MasterState::new(
        Store::open(td.path().join("state.db")).unwrap(),
        Ca::open_or_create(td.path().join("pki")).unwrap(),
        MasterOptions {
            port_range: (20000, 20010),
            admit: sandstorm_core::master::Admission {
                min_free_mib: 0,
                max_psi_avg10: f64::MAX,
            },
            require_node_certs: require_certs,
            ..Default::default()
        },
    ))
}

/// Plain HTTP, with whatever certificate fingerprint we say was presented.
async fn start_with_presented(presented: Option<String>) -> Harness {
    let td = TempDir::new().unwrap();
    let st = state(&td, true);
    let app = router(st.clone()).layer(axum::Extension(PeerCert(presented)));
    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    Harness {
        base: format!("http://{addr}"),
        st,
        _td: td,
    }
}

/// Register a node and return (node_id, its client cert PEM, its key PEM).
async fn register(h: &Harness, node_id: &str) -> (String, String) {
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
            "token": tok["token"],
            "node_id": node_id,
            "fingerprint": "test",
            "max_sandboxes": 10,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    (
        r["client_cert"].as_str().unwrap().to_string(),
        r["client_key"].as_str().unwrap().to_string(),
    )
}

async fn poll_status(base: &str, node_id: &str) -> u16 {
    reqwest::Client::new()
        .post(format!("{base}/v1/node/poll"))
        .json(&serde_json::json!({
            "node_id": node_id, "observed": [], "hold_ms": 0,
            "free_memory_mib": 1024, "pressure_some_avg10": 0.0,
        }))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

// ---- which node is this -------------------------------------------------

#[tokio::test]
async fn identity_a_registered_certificate_may_poll_as_its_own_node() {
    // Registration has to happen before we know the fingerprint to present,
    // so this runs on a harness that presents none, then re-serves with it.
    let setup = start_with_presented(None).await;
    let (cert, _key) = register(&setup, "node-a").await;
    let fp = tls::fingerprint_of_pem(&cert).unwrap();

    // Same store, now with that certificate on the connection.
    let app = router(setup.st.clone()).layer(axum::Extension(PeerCert(Some(fp))));
    let l = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.ok() });

    assert_eq!(poll_status(&format!("http://{addr}"), "node-a").await, 200);
}

#[tokio::test]
async fn identity_one_node_may_not_poll_as_another() {
    let setup = start_with_presented(None).await;
    let (cert_a, _) = register(&setup, "node-a").await;
    register(&setup, "node-b").await;
    let fp_a = tls::fingerprint_of_pem(&cert_a).unwrap();

    let app = router(setup.st.clone()).layer(axum::Extension(PeerCert(Some(fp_a))));
    let l = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.ok() });
    let base = format!("http://{addr}");

    assert_eq!(poll_status(&base, "node-a").await, 200, "its own id");
    assert_eq!(
        poll_status(&base, "node-b").await,
        403,
        "node-a holds a valid certificate, but claiming node-b's id must \
         still be refused -- otherwise it is handed node-b's sandboxes"
    );
}

#[tokio::test]
async fn identity_no_certificate_is_refused_when_required() {
    let h = start_with_presented(None).await;
    register(&h, "node-a").await;
    assert_eq!(poll_status(&h.base, "node-a").await, 401);
}

#[tokio::test]
async fn identity_an_unregistered_certificate_is_refused() {
    // A certificate from a different CA entirely: valid-looking, unknown.
    let other = TempDir::new().unwrap();
    let other_ca = Ca::open_or_create(other.path()).unwrap();
    let stranger = other_ca.issue_node("node-a").unwrap();
    let fp = tls::fingerprint_of_pem(&tls::read_pem(&stranger.cert).unwrap()).unwrap();

    let h = start_with_presented(Some(fp)).await;
    register(&h, "node-a").await;
    assert_eq!(poll_status(&h.base, "node-a").await, 401);
}

#[tokio::test]
async fn identity_revoking_stops_the_node_at_once() {
    let setup = start_with_presented(None).await;
    let (cert, _) = register(&setup, "node-a").await;
    let fp = tls::fingerprint_of_pem(&cert).unwrap();

    let app = router(setup.st.clone()).layer(axum::Extension(PeerCert(Some(fp))));
    let l = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.ok() });
    let base = format!("http://{addr}");

    assert_eq!(poll_status(&base, "node-a").await, 200);

    let revoked = reqwest::Client::new()
        .post(format!("{base}/v1/node/revoke/node-a"))
        .send()
        .await
        .unwrap();
    assert!(revoked.status().is_success(), "revoke");

    // The node still holds the files; what changed is that the master stops
    // accepting them. 403, not 401: the refusal can say why.
    assert_eq!(poll_status(&base, "node-a").await, 403);
}

#[tokio::test]
async fn identity_re_registering_retires_the_old_certificate() {
    let setup = start_with_presented(None).await;
    let (first, _) = register(&setup, "node-a").await;
    let old = tls::fingerprint_of_pem(&first).unwrap();
    let (second, _) = register(&setup, "node-a").await;
    let new = tls::fingerprint_of_pem(&second).unwrap();
    assert_ne!(old, new);

    for (fp, want, what) in [
        (old, 401u16, "the retired one"),
        (new, 200, "the current one"),
    ] {
        let app = router(setup.st.clone()).layer(axum::Extension(PeerCert(Some(fp))));
        let l = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.ok() });
        assert_eq!(
            poll_status(&format!("http://{addr}"), "node-a").await,
            want,
            "{what} should be {want}"
        );
    }
}

#[tokio::test]
async fn identity_without_tls_the_claim_is_waved_through_but_marked() {
    // --insecure-transport: there is no certificate to check, so the id in
    // the body is all there is. The point of the test is that this is a
    // deliberate, separate case rather than a certificate check that
    // silently passes.
    let td = TempDir::new().unwrap();
    let st = state(&td, false);
    let l = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = l.local_addr().unwrap();
    let app = router(st.clone());
    tokio::spawn(async move { axum::serve(l, app).await.ok() });

    let h = Harness {
        base: format!("http://{addr}"),
        st,
        _td: td,
    };
    register(&h, "node-a").await;
    assert_eq!(poll_status(&h.base, "node-a").await, 200);
    // No certificate was presented and none was demanded, which is exactly
    // what the warning in `serve` is about.
}

// ---- does TLS actually happen -------------------------------------------

/// A real handshake: the master serves its certificate, the node presents
/// its own, and the master reads the node's identity off the connection.
#[tokio::test]
async fn transport_a_real_handshake_carries_the_node_identity() {
    tls::install_crypto_provider();
    let td = TempDir::new().unwrap();
    let st = state(&td, true);

    let (cert, key, ca_pem) = {
        let ca = st.ca.lock();
        tls::load_or_issue_master(&ca, vec!["127.0.0.1".into(), "localhost".into()]).unwrap()
    };

    let listener =
        std::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap()).unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let acceptor = tls::PeerCertAcceptor::new(tls::server_config(&cert, &key, &ca_pem).unwrap());
    let app = router(st.clone());
    tokio::spawn(async move {
        axum_server::from_tcp(listener)
            .unwrap()
            .acceptor(acceptor)
            .serve(app.into_make_service())
            .await
            .ok()
    });
    let base = format!("https://127.0.0.1:{}", addr.port());

    // Issue a node identity the way registration does, and record it.
    let node_pem = {
        let ca = st.ca.lock();
        let p = ca.issue_node("node-a").unwrap();
        (
            tls::read_pem(&p.cert).unwrap(),
            tls::read_pem(&p.key).unwrap(),
        )
    };
    st.store()
        .upsert_node(&sandstorm_core::state::Node {
            id: "node-a".into(),
            pool: "default".into(),
            fingerprint: "test".into(),
            max_sandboxes: 10,
            last_seen_at: 0,
            connected: false,
            free_mib: 0,
            psi_some_avg10: 0.0,
        })
        .unwrap();
    st.store()
        .set_node_cert("node-a", &tls::fingerprint_of_pem(&node_pem.0).unwrap())
        .unwrap();

    // The node's client: presents its certificate, verifies the master.
    let node = reqwest::Client::builder()
        .use_preconfigured_tls(tls::client_config(&node_pem.0, &node_pem.1, &ca_pem).unwrap())
        .build()
        .unwrap();
    let r = node
        .post(format!("{base}/v1/node/poll"))
        .json(&serde_json::json!({
            "node_id": "node-a", "observed": [], "hold_ms": 0,
            "free_memory_mib": 1024, "pressure_some_avg10": 0.0,
        }))
        .send()
        .await
        .expect("the handshake and the request should both succeed");
    assert_eq!(r.status().as_u16(), 200, "mTLS poll");

    // And the same connection may not claim another node's id.
    let r = node
        .post(format!("{base}/v1/node/poll"))
        .json(&serde_json::json!({
            "node_id": "node-b", "observed": [], "hold_ms": 0,
            "free_memory_mib": 1024, "pressure_some_avg10": 0.0,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status().as_u16(),
        403,
        "claiming node-b over node-a's TLS"
    );
}

/// A client with no certificate completes the handshake -- registration
/// needs that -- but cannot use the node channel.
#[tokio::test]
async fn transport_a_certless_client_can_register_but_not_poll() {
    tls::install_crypto_provider();
    let td = TempDir::new().unwrap();
    let st = state(&td, true);
    let (cert, key, ca_pem) = {
        let ca = st.ca.lock();
        tls::load_or_issue_master(&ca, vec!["127.0.0.1".into()]).unwrap()
    };
    let listener =
        std::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap()).unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let acceptor = tls::PeerCertAcceptor::new(tls::server_config(&cert, &key, &ca_pem).unwrap());
    let app = router(st.clone());
    tokio::spawn(async move {
        axum_server::from_tcp(listener)
            .unwrap()
            .acceptor(acceptor)
            .serve(app.into_make_service())
            .await
            .ok()
    });
    let base = format!("https://127.0.0.1:{}", addr.port());

    // Verifies the master, presents nothing of its own -- a node that has
    // not registered yet.
    let fresh = reqwest::Client::builder()
        .use_preconfigured_tls(tls::pinned_client_config(
            &tls::fingerprint_of_pem(&ca_pem).unwrap(),
        ))
        .build()
        .unwrap();

    assert!(
        fresh
            .get(format!("{base}/healthz"))
            .send()
            .await
            .unwrap()
            .status()
            .is_success(),
        "an open route is reachable without a certificate"
    );

    let r = fresh
        .post(format!("{base}/v1/node/poll"))
        .json(&serde_json::json!({
            "node_id": "node-a", "observed": [], "hold_ms": 0,
            "free_memory_mib": 1024, "pressure_some_avg10": 0.0,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 401, "no certificate, no node channel");
}

/// The pin is load-bearing: a client that pins one CA must not accept a
/// master holding a certificate from another.
#[tokio::test]
async fn transport_pinning_rejects_a_stand_in_master() {
    tls::install_crypto_provider();
    let real = TempDir::new().unwrap();
    let real_ca = Ca::open_or_create(real.path()).unwrap();
    let real_pin = tls::fingerprint_of_pem(&real_ca.cert_pem()).unwrap();

    // An attacker with its own CA, serving on its own port.
    let fake_dir = TempDir::new().unwrap();
    let fake_ca = Ca::open_or_create(fake_dir.path()).unwrap();
    let (fcert, fkey, fca) = tls::load_or_issue_master(&fake_ca, vec!["127.0.0.1".into()]).unwrap();
    let listener =
        std::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap()).unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let acceptor = tls::PeerCertAcceptor::new(tls::server_config(&fcert, &fkey, &fca).unwrap());
    tokio::spawn(async move {
        axum_server::from_tcp(listener)
            .unwrap()
            .acceptor(acceptor)
            .serve(
                axum::Router::new()
                    .route("/healthz", axum::routing::get(|| async { "ok" }))
                    .into_make_service(),
            )
            .await
            .ok()
    });

    let node = reqwest::Client::builder()
        .use_preconfigured_tls(tls::pinned_client_config(&real_pin))
        .build()
        .unwrap();
    let r = node
        .get(format!("https://127.0.0.1:{}/healthz", addr.port()))
        .send()
        .await;
    assert!(
        r.is_err(),
        "a master that does not hold the pinned CA must fail the handshake, \
         so the registration token is never sent to it"
    );
}

/// Serve the production TLS acceptor while retaining an explicit shutdown path.
fn start_tls(
    st: Arc<MasterState>,
) -> (
    String,
    axum_server::Handle<SocketAddr>,
    tokio::task::JoinHandle<()>,
) {
    let (cert, key, ca_pem) = {
        let ca = st.ca.lock();
        tls::load_or_issue_master(&ca, vec!["127.0.0.1".into()]).unwrap()
    };
    let listener =
        std::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap()).unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("https://{}", listener.local_addr().unwrap());
    let acceptor = tls::PeerCertAcceptor::new(tls::server_config(&cert, &key, &ca_pem).unwrap());
    let handle = axum_server::Handle::new();
    let shutdown = handle.clone();
    let task = tokio::spawn(async move {
        axum_server::from_tcp(listener)
            .unwrap()
            .handle(shutdown)
            .acceptor(acceptor)
            .serve(router(st).into_make_service())
            .await
            .unwrap();
    });
    (base, handle, task)
}

#[tokio::test]
async fn transport_registration_and_node_certificates_survive_master_restart() {
    use sandstorm_core::proto::RegisterResponse;

    tls::install_crypto_provider();
    let td = TempDir::new().unwrap();
    let st = state(&td, true);
    let original_ca = st.ca.lock().cert_pem();
    let original_pin = st.ca_pin().unwrap();
    let (base, shutdown, server) = start_tls(st.clone());
    let bootstrap = reqwest::Client::builder()
        .use_preconfigured_tls(tls::pinned_client_config(&original_pin))
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();

    let mut tokens = Vec::new();
    for _ in 0..2 {
        let response: serde_json::Value = bootstrap
            .post(format!("{base}/v1/node-tokens"))
            .json(&serde_json::json!({ "pool": "default", "ttl_secs": 900 }))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(response["expires_at"].as_i64().unwrap() > sandstorm_core::state::now_secs());
        let token = response["token"].as_str().unwrap().to_string();
        assert_eq!(tls::split_token(&token).0, Some(original_pin.as_str()));
        tokens.push(token);
    }
    let old_node: RegisterResponse = bootstrap
        .post(format!("{base}/v1/node/register"))
        .json(&serde_json::json!({
            "token": tokens[0], "node_id": "old-node",
            "fingerprint": "test", "max_sandboxes": 10,
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(old_node.ca_cert, original_ca);
    let old_client = reqwest::Client::builder()
        .use_preconfigured_tls(
            tls::client_config(&old_node.client_cert, &old_node.client_key, &original_ca).unwrap(),
        )
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();
    assert_eq!(
        old_client
            .get(format!("{base}/ca.pem"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .text()
            .await
            .unwrap(),
        original_ca,
        "the original master certificate verifies against the persisted CA"
    );

    drop(old_client);
    drop(bootstrap);
    shutdown.shutdown();
    server.await.unwrap();
    drop(st);

    let st = state(&td, true);
    let (base, shutdown, server) = start_tls(st.clone());
    let pending_pin = tls::split_token(&tokens[1]).0.unwrap();
    let bootstrap = reqwest::Client::builder()
        .use_preconfigured_tls(tls::pinned_client_config(pending_pin))
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();
    let registration = serde_json::json!({
        "token": tokens[1], "node_id": "new-node",
        "fingerprint": "test", "max_sandboxes": 10,
    });
    let new_node: RegisterResponse = bootstrap
        .post(format!("{base}/v1/node/register"))
        .json(&registration)
        .send()
        .await
        .expect("a pre-restart token must pass the real TLS handshake after restart")
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(new_node.ca_cert, original_ca);
    assert_eq!(st.ca_pin().unwrap(), original_pin);
    assert_eq!(
        bootstrap
            .post(format!("{base}/v1/node/register"))
            .json(&registration)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        401,
        "the preserved registration token remains single-use"
    );

    for node in [&old_node, &new_node] {
        let client = reqwest::Client::builder()
            .use_preconfigured_tls(
                tls::client_config(&node.client_cert, &node.client_key, &original_ca).unwrap(),
            )
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();
        assert_eq!(
            client
                .post(format!("{base}/v1/node/poll"))
                .json(&serde_json::json!({
                    "node_id": node.node_id, "observed": [], "hold_ms": 0,
                    "free_memory_mib": 1024, "pressure_some_avg10": 0.0,
                }))
                .send()
                .await
                .expect("both old and newly issued identities must complete mTLS")
                .status()
                .as_u16(),
            200
        );
        assert_eq!(
            client
                .get(format!("{base}/ca.pem"))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .text()
                .await
                .unwrap(),
            original_ca
        );
    }
    let wrong_pin = "00".repeat(32);
    let wrong_client = reqwest::Client::builder()
        .use_preconfigured_tls(tls::pinned_client_config(&wrong_pin))
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();
    assert!(wrong_client
        .post(format!("{base}/v1/node/register"))
        .json(&registration)
        .send()
        .await
        .is_err());
    shutdown.shutdown();
    server.await.unwrap();
}
