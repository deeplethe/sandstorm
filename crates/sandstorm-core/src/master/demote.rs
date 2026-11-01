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
