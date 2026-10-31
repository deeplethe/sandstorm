//! The idle-demotion and E2B expiry loop.
//!
//! Idle demotion moves a sandbox one tier colder in desired state; E2B
//! expiry pauses or deletes it. The node applies either change when it polls.

use anyhow::Result;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use super::api::MasterState;
use crate::state::now_secs;
use crate::tier::Tier;

pub async fn demotion_loop(st: Arc<MasterState>) -> Result<()> {
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        tick.tick().await;
        let started = std::time::Instant::now();
        let outcome = expire_e2b(&st).and_then(|()| sweep(&st));
        // Timed even when it fails: a sweep that errors after holding the
        // store lock for a second still held it for a second.
        st.metrics.demote_sweep.observe(started.elapsed());
        if let Err(e) = outcome {
            tracing::warn!(error = %e, "demotion sweep failed");
        }
    }
}

fn expire_e2b(st: &Arc<MasterState>) -> Result<()> {
    let mut woken = Vec::new();
    // The store lock serializes expiry with timeout refresh and connect.
    let store = st.store();
    let now = now_secs();
    for (rec, sb) in store.list_e2b()? {
        if rec.end_at > now {
            continue;
        }
        if rec.auto_pause {
            if matches!(sb.tier, Tier::Snapshot | Tier::Detached) {
                continue;
            }
            st.hold_pause(&rec.id);
            store.set_tier(&rec.id, Tier::Snapshot)?;
            tracing::info!(sandbox = %rec.id, "E2B timeout paused sandbox");
        } else {
            store.delete_sandbox(&rec.id)?;
            st.metrics.deletes.fetch_add(1, Relaxed);
            tracing::info!(sandbox = %rec.id, "E2B timeout deleted sandbox");
        }
        woken.push(sb.node_id);
    }
    drop(store);
    woken.sort();
    woken.dedup();
    for node_id in woken {
        st.wake_node(&node_id);
    }
    Ok(())
}

fn sweep(st: &Arc<MasterState>) -> Result<()> {
    let floor = st.idle.to_frozen.as_secs() as i64;
    let mut woken: Vec<String> = Vec::new();
    let store = st.store();
    for (sb, idle) in store.idle_candidates(floor)? {
        // E2B auto-pause keeps its checkpoint until the caller resumes it.
        if sb.tier == Tier::Snapshot
            && store
                .get_e2b(&sb.id)?
                .is_some_and(|(rec, _)| rec.auto_pause && rec.end_at <= now_secs())
        {
            continue;
        }
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
#[path = "demote_tests.rs"]
mod tests;
