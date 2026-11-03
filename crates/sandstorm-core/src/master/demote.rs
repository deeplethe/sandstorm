//! The idle-demotion loop.
//!
//! Only ever moves a sandbox one tier colder, and only in desired state --
//! the node does the work when it polls. Promotion is never decided here;
//! it happens when a request arrives.

use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;

use super::api::MasterState;

pub async fn demotion_loop(st: Arc<MasterState>) -> Result<()> {
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        tick.tick().await;
        let started = std::time::Instant::now();
        let outcome = sweep(&st);
        // Timed even when it fails: a sweep that errors after holding the
        // store lock for a second still held it for a second.
        st.metrics.demote_sweep.observe(started.elapsed());
        if let Err(e) = outcome {
            tracing::warn!(error = %e, "demotion sweep failed");
        }
    }
}

fn sweep(st: &Arc<MasterState>) -> Result<()> {
    let floor = st.idle.to_frozen.as_secs() as i64;
    let mut woken: Vec<String> = Vec::new();
    let store = st.store();
    for (sb, idle) in store.idle_candidates(floor)? {
        let want = st
            .idle
            .tier_for_idle(Duration::from_secs(idle.max(0) as u64));
        // One step at a time, so a long-idle sandbox walks hot -> frozen ->
        // snapshot -> detached over successive sweeps rather than jumping.
        // Each leg is cheap; a jump would stall the loop on a tar.
        let next = match sb.tier.colder() {
            Some(n) => n,
            None => continue,
        };
        if want >= next && sb.tier != want {
            store.set_tier(&sb.id, next)?;
            st.metrics.demoted(next);
            woken.push(sb.node_id.clone());
            tracing::info!(
                sandbox = %sb.id, from = %sb.tier, to = %next, idle_s = idle,
                "demoting"
            );
        }
    }
    drop(store);
    woken.sort();
    woken.dedup();
    for n in woken {
        st.wake_node(&n);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::Router;
    use futures_util::FutureExt;
    use http_body_util::BodyExt;
    use rusqlite::Connection;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use crate::identity::Ca;
    use crate::master::api::{deliver_result, take_execs};
    use crate::master::{router, MasterOptions, SANDBOX_TOKEN_HEADER};
    use crate::proto::{Command, CommandResult};
    use crate::state::{now_secs, Sandbox, Store};
    use crate::tier::{IdlePolicy, Tier};

    struct Harness {
        st: Arc<MasterState>,
        app: Router,
        db: Connection,
        _dir: tempfile::TempDir,
    }

    impl Harness {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("state.db");
            let store = Store::open(&path).unwrap();
            let now = now_secs();
            store
                .insert_sandbox(&Sandbox {
                    id: "sb1".into(),
                    node_id: "node1".into(),
                    pool: "default".into(),
                    template: "python".into(),
                    tier: Tier::Hot,
                    token: "sandbox-token".into(),
                    volume: None,
                    port: None,
                    gateway_port: None,
                    created_at: now,
                    last_active_at: now,
                    last_error: None,
                    error_attempts: 0,
                    net_index: None,
                    ip: None,
                    from_tag: None,
                    memory_limit_mib: None,
                    egress: Default::default(),
                })
                .unwrap();
            let st = Arc::new(MasterState::new(
                store,
                Ca::open_or_create(dir.path().join("pki")).unwrap(),
                MasterOptions {
                    idle: IdlePolicy {
                        to_frozen: Duration::from_secs(100),
                        to_snapshot: Duration::from_secs(200),
                        to_detached: Duration::from_secs(300),
                    },
                    api_token: Some("api-token".into()),
                    ..Default::default()
                },
            ));
            Self {
                app: router(st.clone()),
                st,
                db: Connection::open(path).unwrap(),
                _dir: dir,
            }
        }

        fn sandbox(&self) -> Sandbox {
            self.st.store().get_sandbox("sb1").unwrap().unwrap()
        }

        fn age_by(&self, seconds: i64) {
            // Move stored times together so elapsed idle accumulates unless a request touches it.
            self.db
                .execute(
                    "UPDATE sandboxes SET created_at=created_at-?1, last_active_at=last_active_at-?1",
                    [seconds],
                )
                .unwrap();
        }

        fn sweep_to(&self, tier: Tier) {
            sweep(&self.st).unwrap();
            assert_eq!(self.sandbox().tier, tier);
        }

        fn exec_id(&self) -> String {
            let commands = take_execs(&self.st, "node1");
            assert_eq!(commands.len(), 1);
            match commands.into_iter().next().unwrap() {
                Command::Exec { exec_id, .. } => exec_id,
                other => panic!("expected Exec, got {other:?}"),
            }
        }

        fn complete(&self, result_id: String, exit_code: i64) {
            assert!(deliver_result(
                &self.st,
                CommandResult {
                    result_id,
                    stdout: "done".into(),
                    stderr: "failed".into(),
                    exit_code,
                    size_bytes: 0,
                },
            ));
        }

        async fn rejected(&self, request: Request<Body>, status: StatusCode) {
            let last_active = self.sandbox().last_active_at;
            let response = self.app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), status);
            assert_eq!(self.sandbox().last_active_at, last_active);
            assert!(take_execs(&self.st, "node1").is_empty());
        }
    }

    fn request(endpoint: &str) -> Request<Body> {
        let body = match endpoint {
            "exec" => json!({"args": ["false"]}),
            "eval" => json!({"code": "raise RuntimeError()"}),
            "tier" => json!({"tier": "frozen"}),
            _ => panic!("unknown endpoint {endpoint}"),
        };
        Request::post(format!("/v1/sandboxes/sb1/{endpoint}"))
            .header("content-type", "application/json")
            .header("authorization", "Bearer api-token")
            .header(SANDBOX_TOKEN_HEADER, "sandbox-token")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    async fn continuous_calls_keep_hot(endpoint: &str) {
        let h = Harness::new();
        for exit_code in [0, 1, 0] {
            h.age_by(20);
            h.sweep_to(Tier::Hot);
            let mut response = Box::pin(h.app.clone().oneshot(request(endpoint)));
            assert!(response.as_mut().now_or_never().is_none());
            let exec_id = h.exec_id();
            h.age_by(30);
            h.sweep_to(Tier::Hot);
            h.complete(exec_id, exit_code);
            let response = response.await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let result: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(result["exit_code"], exit_code);
        }
        assert!(now_secs() - h.sandbox().created_at >= 150);

        // Idle starts at completion, and a new execution refreshes it before waiting.
        h.age_by(90);
        h.sweep_to(Tier::Hot);
        let mut response = Box::pin(h.app.clone().oneshot(request(endpoint)));
        assert!(response.as_mut().now_or_never().is_none());
        let exec_id = h.exec_id();
        h.age_by(20);
        h.sweep_to(Tier::Hot);
        h.complete(exec_id, 1);
        assert_eq!(response.await.unwrap().status(), StatusCode::OK);

        h.age_by(90);
        h.sweep_to(Tier::Hot);
        h.age_by(20);
        h.sweep_to(Tier::Frozen);
        h.age_by(100);
        h.sweep_to(Tier::Snapshot);
        h.age_by(100);
        h.sweep_to(Tier::Detached);
    }

    #[tokio::test]
    async fn continuous_exec_stays_hot_then_demotes_after_the_last_result() {
        continuous_calls_keep_hot("exec").await;
    }

    #[tokio::test]
    async fn continuous_eval_stays_hot_then_demotes_after_the_last_result() {
        continuous_calls_keep_hot("eval").await;
    }

    #[tokio::test]
    async fn rejected_exec_and_eval_do_not_extend_idle() {
        for endpoint in ["exec", "eval"] {
            let h = Harness::new();
            h.age_by(90);
            let mut unauthenticated = request(endpoint);
            unauthenticated.headers_mut().remove("authorization");
            h.rejected(unauthenticated, StatusCode::UNAUTHORIZED).await;

            let mut wrong_sandbox_token = request(endpoint);
            wrong_sandbox_token
                .headers_mut()
                .remove(SANDBOX_TOKEN_HEADER);
            h.rejected(wrong_sandbox_token, StatusCode::FORBIDDEN).await;

            let mut invalid = request(endpoint);
            *invalid.body_mut() = Body::from("{}");
            h.rejected(invalid, StatusCode::UNPROCESSABLE_ENTITY).await;
            if endpoint == "exec" {
                let mut empty = request(endpoint);
                *empty.body_mut() = Body::from(r#"{"args":[]}"#);
                h.rejected(empty, StatusCode::BAD_REQUEST).await;
            }

            h.age_by(20);
            h.sweep_to(Tier::Frozen);
            h.rejected(request(endpoint), StatusCode::CONFLICT).await;
            assert_eq!(h.sandbox().tier, Tier::Frozen);
        }
    }

    #[tokio::test]
    async fn late_exec_and_eval_results_do_not_undo_a_manual_freeze() {
        for endpoint in ["exec", "eval"] {
            let h = Harness::new();
            let mut response = Box::pin(h.app.clone().oneshot(request(endpoint)));
            assert!(response.as_mut().now_or_never().is_none());
            let exec_id = h.exec_id();
            let frozen = h.app.clone().oneshot(request("tier")).await.unwrap();
            assert_eq!(frozen.status(), StatusCode::OK);
            h.complete(exec_id, 0);
            assert_eq!(response.await.unwrap().status(), StatusCode::OK);
            assert_eq!(h.sandbox().tier, Tier::Frozen);
        }
    }
}
