//! The E2B surface end to end, short of a real sandbox: the E2B API and
//! front door in front of a real node gateway, with a stand-in for envd
//! where the sandbox would be.
//!
//! What it pins down is the chain a real SDK call goes through -- create
//! waits for envd to take its `/init` with a real token, a sandbox host
//! reaches envd through the gateway with the right credential added and
//! stripped, a stream arrives as a stream -- so a break in any link shows
//! here rather than on a machine with gVisor.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::HeaderMap;
use axum::routing::{get, post};
use parking_lot::Mutex;
use sandstorm_core::identity::Ca;
use sandstorm_core::master::e2b::E2b;
use sandstorm_core::master::{router, MasterOptions, MasterState};
use sandstorm_core::node::{Exposed, Gateway};
use sandstorm_core::state::{Node, Store};
use serde_json::{json, Value};
use tempfile::TempDir;

/// Where envd listens, which the E2B surface hard-codes. The stand-in has
/// to be on it, since the front door asks the gateway for this port.
const ENVD: u16 = 49983;
/// The one gateway port the master may hand out, so the test knows it.
const GATEWAY: u16 = 28471;
const DOMAIN: &str = "e2b.test";

#[derive(Default)]
struct Envd {
    inits: Mutex<Vec<(HeaderMap, Value)>>,
    /// Released by the test once it holds the first chunk of `/stream`.
    release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

async fn fake_envd(state: Arc<Envd>) {
    let app = axum::Router::new()
        .route(
            "/init",
            post({
                let s = state.clone();
                move |h: HeaderMap, body: axum::Json<Value>| async move {
                    s.inits.lock().push((h, body.0));
                    (
                        [("x-envd-version", "0.8.0")],
                        axum::http::StatusCode::NO_CONTENT,
                    )
                }
            }),
        )
        .route(
            "/echo",
            get(|h: HeaderMap| async move {
                let m: serde_json::Map<String, Value> = h
                    .iter()
                    .map(|(k, v)| (k.to_string(), json!(v.to_str().unwrap_or(""))))
                    .collect();
                axum::Json(Value::Object(m))
            }),
        )
        .route(
            "/stream",
            get({
                let s = state.clone();
                move || async move {
                    let rx = s.release.lock().take();
                    let chunks = async_stream(rx);
                    Body::from_stream(chunks)
                }
            }),
        );
    let l = tokio::net::TcpListener::bind(("127.0.0.1", ENVD))
        .await
        .expect("49983 free for the envd stand-in");
    tokio::spawn(async move { axum::serve(l, app).await.ok() });
}

/// "first", then -- only once released -- "second".
fn async_stream(
    rx: Option<tokio::sync::oneshot::Receiver<()>>,
) -> impl futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Send + 'static {
    futures_util::stream::unfold((0u8, rx), |(step, rx)| async move {
        match step {
            0 => Some((Ok(bytes::Bytes::from_static(b"first")), (1, rx))),
            1 => {
                if let Some(rx) = rx {
                    let _ = rx.await;
                }
                Some((Ok(bytes::Bytes::from_static(b"second")), (2, None)))
            }
            _ => None,
        }
    })
}

struct Harness {
    front: String,
    master: String,
    token: String,
    st: Arc<MasterState>,
    envd: Arc<Envd>,
    _td: TempDir,
}

async fn start() -> Harness {
    let td = TempDir::new().unwrap();
    let token = "t".repeat(32);
    let st = Arc::new(MasterState::new(
        Store::open(td.path().join("state.db")).unwrap(),
        Ca::open_or_create(td.path().join("pki")).unwrap(),
        MasterOptions {
            port_range: (GATEWAY, GATEWAY),
            api_token: Some(token.clone()),
            admit: sandstorm_core::master::Admission {
                min_free_mib: 0,
                max_psi_avg10: f64::MAX,
            },
            ..Default::default()
        },
    ));
    {
        let store = st.store();
        store
            .upsert_node(&Node {
                id: "n1".into(),
                pool: "default".into(),
                fingerprint: "fp".into(),
                max_sandboxes: 10,
                last_seen_at: 0,
                connected: true,
                free_mib: 4096,
                psi_some_avg10: 0.0,
            })
            .unwrap();
        // How the front door learns where the node's gateway is.
        store.set_node_agent("n1", "127.0.0.1:1", "x", "x").unwrap();
        store
            .put_template(
                "base",
                "rootfs",
                r#"{"args":["/bin/true"],"memory_max_mib":256}"#,
            )
            .unwrap();
    }

    let envd = Arc::new(Envd::default());
    fake_envd(envd.clone()).await;

    // The node's half: expose whatever the master has placed, the way the
    // node does after each poll.
    {
        let st = st.clone();
        tokio::spawn(async move {
            let mut gw = Gateway::new();
            loop {
                let want: Vec<Exposed> = st
                    .store()
                    .list_sandboxes()
                    .unwrap()
                    .into_iter()
                    .filter_map(|s| {
                        Some(Exposed {
                            id: s.id,
                            gateway_port: s.gateway_port?,
                            sandbox_ip: "127.0.0.1".parse().unwrap(),
                            sandbox_port: s.port.unwrap_or(8080),
                            token: s.token,
                        })
                    })
                    .collect();
                gw.reconcile(&want).await;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });
    }

    let serve = |app: axum::Router| async move {
        let l = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.ok() });
        format!("http://{addr}")
    };
    let front = serve(sandstorm_core::master::front::router(E2b::new(
        st.clone(),
        DOMAIN,
    )))
    .await;
    let master = serve(router(st.clone())).await;
    Harness {
        front,
        master,
        token,
        st,
        envd,
        _td: td,
    }
}

impl Harness {
    /// A request to the front door as if for `host`.
    fn req(&self, method: reqwest::Method, host: &str, path: &str) -> reqwest::RequestBuilder {
        reqwest::Client::new()
            .request(method, format!("{}{path}", self.front))
            .header("host", host)
            .header("x-api-key", &self.token)
    }
    fn api(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.req(method, &format!("api.{DOMAIN}"), path)
    }
}

#[tokio::test]
async fn the_e2b_surface_end_to_end() {
    let h = start().await;

    // Unauthenticated: refused, in E2B's error shape.
    let r = reqwest::Client::new()
        .get(format!("{}/sandboxes", h.front))
        .header("host", format!("api.{DOMAIN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let b: Value = r.json().await.unwrap();
    assert_eq!(b["code"], 401, "{b}");
    assert!(b["message"].is_string(), "{b}");

    // An unknown template is named in the refusal.
    let r = h
        .api(reqwest::Method::POST, "/sandboxes")
        .json(&json!({"templateID": "nope"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    // Create: answered only once envd has its /init. Through /v2, as the
    // SDKs have since 2.51; older ones POST the same body to /sandboxes.
    let r = h
        .api(reqwest::Method::POST, "/v2/sandboxes")
        .json(&json!({"templateID": "base", "timeout": 120, "envVars": {"FOO": "bar"}, "metadata": {"who": "test"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let made: Value = r.json().await.unwrap();
    let id = made["sandboxID"].as_str().unwrap().to_string();
    let access = made["envdAccessToken"].as_str().unwrap().to_string();
    assert_eq!(made["domain"], DOMAIN);
    assert_eq!(made["envdVersion"], "0.8.0", "taken from envd's own answer");
    assert_eq!(made["clientID"].as_str().unwrap().len(), 8);
    assert_eq!(access.len(), 32, "a real token, not a placeholder");

    {
        let inits = h.envd.inits.lock();
        assert_eq!(inits.len(), 1);
        let (headers, body) = &inits[0];
        assert_eq!(body["accessToken"], access.as_str());
        assert_eq!(body["envVars"], json!({"FOO": "bar"}));
        assert_eq!(
            body["defaultUser"], "user",
            "E2B's default user, not envd's root"
        );
        assert!(
            headers.get("x-sandstorm-sandbox-token").is_none(),
            "the gateway strips its credential"
        );
        assert!(headers.get("x-sandstorm-port").is_none());
    }

    // One id, both APIs.
    let r = reqwest::Client::new()
        .get(format!("{}/v1/sandboxes/{id}", h.master))
        .header("authorization", format!("Bearer {}", h.token))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "the E2B id is the Sandstorm id");
    let detail: Value = h
        .api(reqwest::Method::GET, &format!("/sandboxes/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detail["state"], "running");
    assert_eq!(detail["metadata"], json!({"who": "test"}));
    assert_eq!(detail["envdAccessToken"], access.as_str());

    // Through the front door by Host: the credential is added for the
    // gateway, taken off before envd, and the caller's own headers reach it.
    let host = format!("{ENVD}-{id}.{DOMAIN}");
    let seen: Value = h
        .req(reqwest::Method::GET, &host, "/echo")
        .header("x-access-token", &access)
        .header("x-sandstorm-sandbox-token", "forged")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(seen["x-access-token"], access.as_str());
    assert!(seen.get("x-sandstorm-sandbox-token").is_none(), "{seen}");
    assert!(seen.get("x-sandstorm-port").is_none(), "{seen}");

    // The same by path, and by E2B's headers.
    let r = h
        .req(
            reqwest::Method::GET,
            "127.0.0.1",
            &format!("/sandbox/{id}/{ENVD}/echo"),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = h
        .req(reqwest::Method::GET, "localhost", "/echo")
        .header("e2b-sandbox-id", &id)
        .header("e2b-sandbox-port", ENVD.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // A stream arrives as one: the first chunk is in hand before envd has
    // written the second.
    let (tx, rx) = tokio::sync::oneshot::channel();
    *h.envd.release.lock() = Some(rx);
    let mut r = h
        .req(reqwest::Method::GET, &host, "/stream")
        .send()
        .await
        .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), r.chunk())
        .await
        .expect("the first chunk arrives while the stream is still open")
        .unwrap()
        .unwrap();
    assert_eq!(&first[..], b"first");
    tx.send(()).unwrap();
    let second = r.chunk().await.unwrap().unwrap();
    assert_eq!(&second[..], b"second");

    // Deadlines.
    let r = h
        .api(reqwest::Method::POST, &format!("/sandboxes/{id}/timeout"))
        .json(&json!({"timeout": 5}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let r = h
        .api(
            reqwest::Method::POST,
            &format!("/v2/sandboxes/{id}/connect"),
        )
        .json(&json!({"timeout": 600}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        200,
        "a running sandbox is connected to, not resumed"
    );
    let c: Value = r.json().await.unwrap();
    assert_eq!(c["envdAccessToken"], access.as_str());

    // Pause: paused at once in the E2B view, and a second pause is a 409.
    let r = h
        .api(reqwest::Method::POST, &format!("/sandboxes/{id}/pause"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let d: Value = h
        .api(reqwest::Method::GET, &format!("/sandboxes/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(d["state"], "paused");
    let r = h
        .api(reqwest::Method::POST, &format!("/sandboxes/{id}/pause"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409);

    // Traffic from just before the pause, reported just after it, is not a
    // wake: measured on bm, it turned beta_pause into a no-op.
    let r = reqwest::Client::new()
        .post(format!("{}/v1/node/traffic", h.master))
        .header("authorization", format!("Bearer {}", h.token))
        .json(&json!({
            "node_id": "n1",
            "observed": [{"id": id, "tier": "hot", "serving": true, "ip": null, "net_index": null}],
            "free_memory_mib": 2000, "pressure_some_avg10": 0.0,
            "traffic": [id],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let d: Value = h
        .api(reqwest::Method::GET, &format!("/sandboxes/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        d["state"], "paused",
        "a late traffic report undid the pause"
    );

    // The list filters by state; plain /sandboxes is the running ones.
    let paused: Value = h
        .api(reqwest::Method::GET, "/v2/sandboxes?state=paused")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(paused.as_array().unwrap().len(), 1);
    let running: Value = h
        .api(reqwest::Method::GET, "/v2/sandboxes?state=running")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(running.as_array().unwrap().is_empty());
    let v1: Value = h
        .api(reqwest::Method::GET, "/sandboxes")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(v1.as_array().unwrap().is_empty());

    // Resume: back to running, and envd re-given the same token.
    let r = h
        .api(reqwest::Method::POST, &format!("/sandboxes/{id}/connect"))
        .json(&json!({"timeout": 60}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "a paused sandbox is resumed");
    assert_eq!(
        h.envd.inits.lock().last().unwrap().1["accessToken"],
        access.as_str()
    );

    // Templates by name.
    let t: Value = h
        .api(reqwest::Method::GET, "/templates")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(t[0]["templateID"], "base");

    // A sandbox made through /v1 is not reachable through the front door,
    // which would otherwise add its token for anyone who knew its id.
    let native: Value = reqwest::Client::new()
        .post(format!("{}/v1/sandboxes", h.master))
        .header("authorization", format!("Bearer {}", h.token))
        .json(&json!({"template": "base"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let nid = native["id"].as_str().unwrap();
    let r = h
        .req(reqwest::Method::GET, &format!("8080-{nid}.{DOMAIN}"), "/")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 502);

    // Kill: gone from both views, and from the front door.
    let r = h
        .api(reqwest::Method::DELETE, &format!("/sandboxes/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let r = h
        .api(reqwest::Method::GET, &format!("/sandboxes/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    assert!(h.st.store().get_sandbox(&id).unwrap().is_none());
    let r = h
        .req(reqwest::Method::GET, &host, "/echo")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 502);
}
