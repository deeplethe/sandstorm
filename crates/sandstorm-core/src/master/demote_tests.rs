use super::*;
use crate::identity::Ca;
use crate::master::api::MasterOptions;
use crate::proto::{ObservedSandbox, PollRequest};
use crate::state::{E2bSandbox, Sandbox, Store};
use tempfile::TempDir;

fn state(dir: &TempDir) -> Arc<MasterState> {
    Arc::new(MasterState::new(
        Store::open(dir.path().join("state.db")).unwrap(),
        Ca::open_or_create(dir.path().join("pki")).unwrap(),
        MasterOptions::default(),
    ))
}

fn add_e2b(st: &Arc<MasterState>, id: &str, auto_pause: bool, end_at: i64) {
    let store = st.store();
    store
        .insert_sandbox(&Sandbox {
            id: id.into(),
            node_id: "node".into(),
            pool: "default".into(),
            template: "base".into(),
            tier: Tier::Hot,
            token: "sandbox-token".into(),
            volume: None,
            port: None,
            gateway_port: None,
            created_at: now_secs(),
            last_active_at: now_secs() - 2 * 60 * 60,
            last_error: None,
            error_attempts: 0,
            net_index: None,
            ip: None,
            from_tag: None,
            memory_limit_mib: None,
            egress: Default::default(),
        })
        .unwrap();
    store
        .insert_e2b(&E2bSandbox {
            id: id.into(),
            client_id: format!("client-{id}"),
            template_id: "base".into(),
            access_token: "access-token".into(),
            traffic_access_token: None,
            envd_version: "".into(),
            env_vars: "{}".into(),
            metadata: "{}".into(),
            auto_pause,
            end_at,
        })
        .unwrap();
}

#[test]
fn expired_deadlines_survive_restart_and_renewal_wins() {
    let dir = TempDir::new().unwrap();
    let first = state(&dir);
    let past = now_secs() - 1;
    add_e2b(&first, "pause", true, past);
    add_e2b(&first, "delete", false, past);
    add_e2b(&first, "renew", false, past);
    drop(first);

    let restarted = state(&dir);
    restarted
        .store()
        .set_e2b_end_at("renew", now_secs() + 60)
        .unwrap();
    expire_e2b(&restarted).unwrap();
    let store = restarted.store();
    assert_eq!(
        store.get_sandbox("pause").unwrap().unwrap().tier,
        Tier::Snapshot
    );
    assert!(store.get_sandbox("delete").unwrap().is_none());
    assert!(store.get_e2b("delete").unwrap().is_none());
    assert!(store.get_sandbox("renew").unwrap().is_some());
    drop(store);

    // A paused sandbox keeps its record so connect can resume it.
    expire_e2b(&restarted).unwrap();
    sweep(&restarted).unwrap();
    sweep(&restarted).unwrap();
    assert_eq!(
        restarted
            .store()
            .get_sandbox("pause")
            .unwrap()
            .unwrap()
            .tier,
        Tier::Snapshot
    );
    assert!(restarted.store().get_e2b("pause").unwrap().is_some());

    // A late gateway report must not undo the expired auto-pause.
    crate::master::api::apply_traffic(
        &restarted,
        &PollRequest {
            node_id: "node".into(),
            observed: vec![ObservedSandbox {
                id: "pause".into(),
                tier: Tier::Hot,
                serving: true,
                ip: None,
                net_index: None,
                egress: Default::default(),
            }],
            traffic: vec!["pause".into()],
            free_memory_mib: 0,
            pressure_some_avg10: 0.0,
            hold_ms: None,
            failures: vec![],
            deferred: vec![],
            pulls: vec![],
            evicted: vec![],
            features: vec![],
            tags: vec![],
            host: None,
            fingerprint: None,
            templates: None,
        },
    )
    .unwrap();
    assert_eq!(
        restarted
            .store()
            .get_sandbox("pause")
            .unwrap()
            .unwrap()
            .tier,
        Tier::Snapshot
    );
}
