//! Which backend a node runs, as the master sees it: the backend a join
//! command asks for, templates placed only where a node's backend can start
//! them, and moving an empty node to another backend. The node side of the
//! switch is `node/switch.rs`; here a test plays the node over the poll.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use sandstorm_core::identity::Ca;
use sandstorm_core::master::{router, MasterState};
use sandstorm_core::proto::{Command, PollResponse};
use sandstorm_core::state::Store;
use serde_json::{json, Value};
use tempfile::TempDir;

struct Harness {
    base: String,
    c: reqwest::Client,
    _td: TempDir,
}

async fn start() -> Harness {
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
    let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(st)).await.ok();
    });
    Harness {
        base: format!("http://{addr}"),
        c: reqwest::Client::new(),
        _td: td,
    }
}

impl Harness {
    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.c
            .post(format!("{}{path}", self.base))
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    async fn get(&self, path: &str) -> Value {
        self.c
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn register(&self, node: &str, fingerprint: &str) {
        let tok: Value = self
            .post("/v1/node-tokens", json!({}))
            .await
            .json()
            .await
            .unwrap();
        let r = self
            .post(
                "/v1/node/register",
                json!({ "token": tok["token"], "node_id": node,
                        "fingerprint": fingerprint, "max_sandboxes": 100 }),
            )
            .await;
        assert_eq!(r.status(), 200);
    }

    /// One report from `node`, on `engine`, saying it can start `templates`
    /// and can switch if `switchable`. Returns the commands it is given.
    async fn poll(
        &self,
        node: &str,
        engine: &str,
        templates: Option<&[&str]>,
        switchable: bool,
    ) -> Vec<Command> {
        let mut body = json!({
            "node_id": node, "observed": [],
            "free_memory_mib": 4000, "pressure_some_avg10": 0.0, "hold_ms": 0,
            "host": { "engine": engine, "kernel": "6.8", "cpu_vendor": "x",
                      "cpu_model": "y", "cpu_flags": "z" },
            "features": if switchable { vec!["backend-switch"] } else { vec!["reclaim-hot"] },
        });
        if let Some(t) = templates {
            body["templates"] = json!(t);
        }
        let r: PollResponse = self.post("/v1/node/poll", body).await.json().await.unwrap();
        r.commands
    }

    async fn template(&self, name: &str) {
        let r = self
            .post(
                "/v1/templates",
                json!({ "name": name, "rootfs": "rootfs",
                        "spec": { "args": ["/bin/true"], "memory_max_mib": 64 } }),
            )
            .await;
        assert_eq!(r.status(), 201);
    }
}

const RUNSC: &str = "runsc version release-20261019.0";
const FC: &str = "Firecracker v1.10.1";

#[tokio::test]
async fn the_join_command_carries_the_backend_asked_for() {
    let h = start().await;
    let r: Value = h
        .post(
            "/v1/node-bootstrap",
            json!({ "master_url": "https://m:8443", "backend": "firecracker",
                    "fc_kernel": "/var/lib/sandstorm/kernels/vmlinux",
                    "state_dir": "/data/sandstorm" }),
        )
        .await
        .json()
        .await
        .unwrap();
    let cmd = r["install_command"].as_str().unwrap();
    assert!(cmd.contains(" SANDSTORM_BACKEND=firecracker"), "{cmd}");
    assert!(
        cmd.contains(" SANDSTORM_FC_KERNEL=/var/lib/sandstorm/kernels/vmlinux"),
        "{cmd}"
    );
    assert!(
        cmd.contains(" SANDSTORM_STATE_DIR=/data/sandstorm"),
        "{cmd}"
    );
    assert!(
        cmd.ends_with(" sh"),
        "the variables go before the shell: {cmd}"
    );

    // Without one, the command is what it was: the installer's own default.
    let r: Value = h
        .post(
            "/v1/node-bootstrap",
            json!({ "master_url": "https://m:8443" }),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(!r["install_command"]
        .as_str()
        .unwrap()
        .contains("SANDSTORM_BACKEND"));
}

#[tokio::test]
async fn a_join_command_cannot_smuggle_shell_into_root() {
    let h = start().await;
    for bad in [
        json!({ "master_url": "https://m", "backend": "kata" }),
        json!({ "master_url": "https://m", "fc_kernel": "/x; rm -rf /" }),
        json!({ "master_url": "https://m", "state_dir": "relative/dir" }),
        json!({ "master_url": "https://m", "state_dir": "/a/../etc" }),
        json!({ "master_url": "https://m", "fc_kernel": "/a b" }),
    ] {
        let r = h.post("/v1/node-bootstrap", bad.clone()).await;
        assert_eq!(r.status(), 400, "{bad}");
    }
}

/// A template is a rootfs directory to runsc and an ext4 image to
/// Firecracker, so a pool that mixes the two may have only one form. The
/// sandbox goes to a node that has it, and when none has, the create is
/// refused saying why instead of failing on a node.
#[tokio::test]
async fn a_template_goes_only_where_its_backend_has_it() {
    let h = start().await;
    h.register("gv", "fp-gvisor").await;
    h.register("fc", "fp-firecracker").await;
    h.poll("gv", RUNSC, Some(&["web"]), true).await;
    h.poll("fc", FC, Some(&["web-ext4"]), true).await;
    h.template("web").await;
    h.template("web-ext4").await;
    h.template("nowhere").await;

    for (t, node) in [("web", "gv"), ("web-ext4", "fc")] {
        for _ in 0..3 {
            let r: Value = h
                .post("/v1/sandboxes", json!({ "template": t }))
                .await
                .json()
                .await
                .unwrap();
            assert_eq!(r["node_id"], node, "{t}: {r}");
        }
    }

    let r = h
        .post("/v1/sandboxes", json!({ "template": "nowhere" }))
        .await;
    assert_eq!(r.status(), 409);
    let e: Value = r.json().await.unwrap();
    let msg = e["error"].as_str().unwrap();
    assert!(msg.contains("rootfs.ext4"), "names both forms: {msg}");

    let r = h
        .post(
            "/v1/sandboxes",
            json!({ "template": "web", "node_id": "fc" }),
        )
        .await;
    assert_eq!(r.status(), 409, "asked for by name, refused by name");
}

/// A node that has never said which templates it has -- one from before
/// nodes said -- is placed on as it always was.
#[tokio::test]
async fn a_node_that_has_not_said_takes_any_template() {
    let h = start().await;
    h.register("old", "fp").await;
    h.poll("old", RUNSC, None, false).await;
    h.template("web").await;
    let r = h.post("/v1/sandboxes", json!({ "template": "web" })).await;
    assert_eq!(r.status(), 201);
}

#[tokio::test]
async fn the_console_shows_what_each_node_can_start_and_whether_it_can_switch() {
    let h = start().await;
    h.register("gv", "fp").await;
    h.poll("gv", RUNSC, Some(&["web"]), true).await;
    let now = h.get("/v1/console/now").await;
    let n = &now["nodes"][0];
    assert_eq!(n["templates"], json!(["web"]));
    assert_eq!(n["snapshots"], 0);
    assert!(n["features"]
        .as_array()
        .unwrap()
        .contains(&json!("backend-switch")));
    assert!(n.get("switching_to").is_none());
}

#[tokio::test]
async fn a_node_holding_a_sandbox_cannot_switch() {
    let h = start().await;
    h.register("gv", "fp").await;
    h.poll("gv", RUNSC, Some(&["web"]), true).await;
    h.template("web").await;
    let made = h.post("/v1/sandboxes", json!({ "template": "web" })).await;
    assert_eq!(made.status(), 201);

    let r = h
        .post("/v1/nodes/gv/backend", json!({ "backend": "firecracker" }))
        .await;
    assert_eq!(r.status(), 409);
    let e: Value = r.json().await.unwrap();
    assert!(e["error"].as_str().unwrap().contains("1 sandbox"), "{e}");
    // Refused, so it is placeable again at once.
    let again = h.post("/v1/sandboxes", json!({ "template": "web" })).await;
    assert_eq!(again.status(), 201);
}

#[tokio::test]
async fn a_node_installed_without_a_backend_file_cannot_switch() {
    let h = start().await;
    h.register("gv", "fp").await;
    h.poll("gv", RUNSC, Some(&[]), false).await;
    let r = h
        .post("/v1/nodes/gv/backend", json!({ "backend": "firecracker" }))
        .await;
    assert_eq!(r.status(), 409);
    let e: Value = r.json().await.unwrap();
    assert!(e["error"].as_str().unwrap().contains("backend file"), "{e}");
}

#[tokio::test]
async fn switching_to_the_backend_it_has_does_nothing() {
    let h = start().await;
    h.register("gv", "fp").await;
    h.poll("gv", RUNSC, Some(&[]), true).await;
    let r: Value = h
        .post("/v1/nodes/gv/backend", json!({ "backend": "gvisor" }))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(r["restarting"], false);
}

#[tokio::test]
async fn an_unknown_backend_is_refused() {
    let h = start().await;
    h.register("gv", "fp").await;
    h.poll("gv", RUNSC, Some(&[]), true).await;
    let r = h
        .post("/v1/nodes/gv/backend", json!({ "backend": "kata" }))
        .await;
    assert_eq!(r.status(), 400);
}

/// The whole switch, the node played by hand: the master queues the
/// command, keeps the node out of placement while it restarts, takes the
/// node's answer, and lets it back in once it reports the new engine.
#[tokio::test]
async fn an_empty_node_is_switched_and_kept_out_of_placement_meanwhile() {
    let h = start().await;
    h.register("gv", "fp").await;
    h.poll("gv", RUNSC, Some(&["web"]), true).await;
    h.template("web").await;

    let base = h.base.clone();
    let ask = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("{base}/v1/nodes/gv/backend"))
            .json(&json!({ "backend": "firecracker" }))
            .send()
            .await
            .unwrap()
    });

    // The node's next poll carries the command.
    let mut result_id = None;
    for _ in 0..50 {
        for c in h.poll("gv", RUNSC, Some(&["web"]), true).await {
            if let Command::SetBackend {
                backend,
                result_id: r,
            } = c
            {
                assert_eq!(backend, "firecracker");
                result_id = Some(r);
            }
        }
        if result_id.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let result_id = result_id.expect("the switch reached the node");

    // While it is under way nothing is placed on it.
    let r = h.post("/v1/sandboxes", json!({ "template": "web" })).await;
    assert_eq!(r.status(), 503, "placed on a node about to restart");
    let now = h.get("/v1/console/now").await;
    assert_eq!(now["nodes"][0]["switching_to"], "firecracker");

    let r = h
        .post(
            "/v1/node/result",
            json!({ "result_id": result_id, "stdout": "switching", "exit_code": 0 }),
        )
        .await;
    assert_eq!(r.status(), 204);
    let answer: Value = ask.await.unwrap().json().await.unwrap();
    assert_eq!(answer["restarting"], true, "{answer}");
    assert_eq!(answer["from"], "runsc");
    assert_eq!(answer["to"], "firecracker");

    // Back on Firecracker, with the ext4 form of the template: placeable.
    h.poll("gv", FC, Some(&["web"]), true).await;
    let now = h.get("/v1/console/now").await;
    assert!(now["nodes"][0].get("switching_to").is_none());
    let r = h.post("/v1/sandboxes", json!({ "template": "web" })).await;
    assert_eq!(r.status(), 201);
}

/// What the node refused with comes back to whoever asked, and the node is
/// placeable again.
#[tokio::test]
async fn a_switch_the_node_refuses_says_why() {
    let h = start().await;
    h.register("gv", "fp").await;
    h.poll("gv", RUNSC, Some(&["web"]), true).await;
    h.template("web").await;
    let base = h.base.clone();
    let ask = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("{base}/v1/nodes/gv/backend"))
            .json(&json!({ "backend": "firecracker" }))
            .send()
            .await
            .unwrap()
    });
    let mut result_id = None;
    for _ in 0..50 {
        for c in h.poll("gv", RUNSC, Some(&["web"]), true).await {
            if let Command::SetBackend { result_id: r, .. } = c {
                result_id = Some(r);
            }
        }
        if result_id.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    h.post(
        "/v1/node/result",
        json!({ "result_id": result_id.unwrap(),
                "stderr": "this host cannot run firecracker: no guest kernel",
                "exit_code": 1 }),
    )
    .await;
    let r = ask.await.unwrap();
    assert_eq!(r.status(), 409);
    let e: Value = r.json().await.unwrap();
    assert!(
        e["error"].as_str().unwrap().contains("no guest kernel"),
        "{e}"
    );
    let r = h.post("/v1/sandboxes", json!({ "template": "web" })).await;
    assert_eq!(r.status(), 201);
}

/// A create can name the backend, which is what decides whether a template
/// runs: a pool's host groups may mix gVisor and Firecracker.
#[tokio::test]
async fn a_create_can_ask_for_a_backend() {
    let h = start().await;
    h.register("gv", "fp-gvisor").await;
    h.register("fc", "fp-firecracker").await;
    h.poll("gv", RUNSC, Some(&["web"]), true).await;
    h.poll("fc", FC, Some(&["web"]), true).await;
    h.template("web").await;
    for (b, node) in [("firecracker", "fc"), ("gvisor", "gv")] {
        for _ in 0..3 {
            let r: Value = h
                .post("/v1/sandboxes", json!({ "template": "web", "backend": b }))
                .await
                .json()
                .await
                .unwrap();
            assert_eq!(r["node_id"], node, "{b}: {r}");
        }
    }
    let r = h
        .post(
            "/v1/sandboxes",
            json!({ "template": "web", "backend": "firecracker", "node_id": "gv" }),
        )
        .await;
    assert_eq!(r.status(), 409);
    let r = h
        .post(
            "/v1/sandboxes",
            json!({ "template": "web", "backend": "kata" }),
        )
        .await;
    assert_eq!(r.status(), 400);
}
