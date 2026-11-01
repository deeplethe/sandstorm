//! Driving a node that listens.
//!
//! The mirror of `poll`. A polling node opens the connection and the master
//! answers it with commands. Here the master opens it: it sends the commands
//! and reads the node's report in the reply. In between it runs the same
//! three steps as `poll` -- record the node's failures, diff desired state
//! against what the node reports, drain the node's exec queue -- so a
//! sandbox behaves the same whichever way its node is connected.
//!
//! One task per node. It wakes when desired state for the node changes
//! (the same waker a parked poll listens on), when the node has a retry
//! due, or every `REFRESH` regardless, which is what keeps the node's
//! memory and pressure figures current for admission.

use anyhow::{bail, Context, Result};
use std::time::Duration;

use super::api::{
    apply_evictions, apply_traffic, deliver_result, diff, record_failures, take_execs, St,
};
use crate::proto::{AgentHello, Command, PollRequest, SyncRequest, SyncResponse};
use crate::state::AgentNode;

/// How often an idle node is asked for a fresh report.
///
/// This is the age of the free-memory figure admission decides with, so it
/// is the window a burst of placements can overshoot in. A polling node's
/// equivalent is its hold, 25 s by default; this is shorter because the
/// master pays for it, not the node, and the report is a few hundred bytes.
const REFRESH: Duration = Duration::from_secs(5);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// A batch's own budget before exec time is added.
const SYNC_BASE: Duration = Duration::from_secs(60);
const BACKOFF_MIN: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(15);

/// A client that trusts exactly the certificate the pairing code pins.
pub(super) fn client(cert_sha256: &str) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .use_preconfigured_tls(crate::tls::pinned_leaf_client_config(cert_sha256))
        .connect_timeout(CONNECT_TIMEOUT)
        .build()?)
}

/// First contact: prove the pairing code works and learn what the node is.
pub(super) async fn hello(address: &str, cert_sha256: &str, secret: &str) -> Result<AgentHello> {
    let r = client(cert_sha256)?
        .get(format!("https://{address}/v1/agent/hello"))
        .bearer_auth(secret)
        .timeout(CONNECT_TIMEOUT)
        .send()
        .await
        // The usual cause, by far, is a port the security group drops. Say
        // what was dialled so that is easy to check.
        .with_context(|| format!("connecting to https://{address}"))?;
    let status = r.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        bail!("the node refused the secret: this pairing code is not this node's");
    }
    if !status.is_success() {
        bail!(
            "the node answered {status}: {}",
            r.text().await.unwrap_or_default()
        );
    }
    Ok(r.json().await?)
}

/// Start driving a node, replacing any driver it already had.
pub(super) fn start(st: St, node: AgentNode) {
    let id = node.id.clone();
    let handle = tokio::spawn(drive(st.clone(), node));
    st.set_driver(&id, handle.abort_handle());
}

/// Commands for the node, computed exactly as `poll` computes them.
fn next_batch(st: &St, report: &PollRequest) -> Result<Vec<Command>> {
    let mut commands = diff(st, report).map_err(|(_, e)| anyhow::anyhow!("{}", e.0))?;
    commands.extend(take_execs(st, &report.node_id));
    Ok(commands)
}

/// Long enough for every exec in the batch to run to its own timeout.
fn sync_timeout(batch: &Option<Vec<Command>>) -> Duration {
    let execs = batch
        .iter()
        .flatten()
        .map(|c| match c {
            Command::Exec { timeout_secs, .. } => *timeout_secs,
            _ => 0,
        })
        .sum::<u64>();
    SYNC_BASE + Duration::from_secs(execs)
}

async fn sync_once(
    client: &reqwest::Client,
    url: &str,
    secret: &str,
    batch: &Option<Vec<Command>>,
) -> Result<SyncResponse> {
    let r = client
        .post(url)
        .bearer_auth(secret)
        .json(&SyncRequest {
            commands: batch.clone(),
        })
        .timeout(sync_timeout(batch))
        .send()
        .await?;
    let status = r.status();
    if !status.is_success() {
        bail!("{status}: {}", r.text().await.unwrap_or_default());
    }
    Ok(r.json().await?)
}

async fn drive(st: St, node: AgentNode) {
    let client = match client(&node.cert_sha256) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(node = %node.id, error = %e, "cannot build a client for this node");
            return;
        }
    };
    let url = format!("https://{}/v1/agent/sync", node.address);
    let waker = st.waker(&node.id);
    let mut batch: Option<Vec<Command>> = None;
    let mut backoff = BACKOFF_MIN;
    let mut up = false;

    loop {
        let resp = match sync_once(&client, &url, &node.secret, &batch).await {
            Ok(r) => r,
            Err(e) => {
                if up {
                    tracing::warn!(node = %node.id, address = %node.address, error = %format!("{e:#}"), "lost the node");
                } else {
                    tracing::debug!(node = %node.id, error = %format!("{e:#}"), "node not answering");
                }
                up = false;
                let _ = st.store().set_node_connected(&node.id, false);
                // Execs in a batch that never arrived are lost, exactly as
                // they are when a poll response never arrives: the caller
                // times out. Declarative commands are not -- the next diff
                // issues them again.
                batch = None;
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
                continue;
            }
        };
        if !up {
            tracing::info!(node = %node.id, address = %node.address, "driving node");
            up = true;
        }
        backoff = BACKOFF_MIN;

        for r in resp.results {
            deliver_result(&st, r);
        }
        let mut report = resp.report;
        // Which node this is was settled when it was added: the one at this
        // address presenting this certificate. Not whatever the body says.
        report.node_id = node.id.clone();
        if let Err((_, e)) = record_failures(&st, &report) {
            tracing::warn!(node = %node.id, error = %e.0, "could not record failures");
        }
        if let Err((_, e)) = apply_traffic(&st, &report) {
            tracing::warn!(node = %node.id, error = %e.0, "could not record traffic");
        }
        if let Err((_, e)) = apply_evictions(&st, &report) {
            tracing::warn!(node = %node.id, error = %e.0, "could not record evictions");
        }

        let sent = batch.take();
        let mut next = match next_batch(&st, &report) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(node = %node.id, error = %format!("{e:#}"), "diff failed");
                Vec::new()
            }
        };

        // Wait when there is nothing to do -- and also when the only thing
        // to do is what was just sent. Diff re-issues a command for every
        // sandbox that has not converged, including ones the node is
        // backing off on, so without this the driver would resend the same
        // batch at once, the node would skip every command in it, and the
        // two would spin until the retry was due.
        let nothing_new = next.is_empty() || sent.as_ref().is_some_and(|s| same_batch(s, &next));
        if nothing_new {
            let wait = resp
                .retry_in_ms
                .map(Duration::from_millis)
                .map_or(REFRESH, |d| d.min(REFRESH));
            tokio::time::timeout(wait, waker.notified()).await.ok();
            next = next_batch(&st, &report).unwrap_or_default();
        }
        // Nothing to send is a report-only refresh, which leaves the node's
        // failure book alone and keeps its figures current.
        batch = if next.is_empty() { None } else { Some(next) };
    }
}

fn same_batch(a: &[Command], b: &[Command]) -> bool {
    a.len() == b.len() && serde_json::to_string(a).ok() == serde_json::to_string(b).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_batch_gets_time_for_every_exec_in_it() {
        let exec = |t| Command::Exec {
            exec_id: "e".into(),
            id: "sb".into(),
            args: vec!["true".into()],
            timeout_secs: t,
        };
        assert_eq!(sync_timeout(&None), SYNC_BASE);
        assert_eq!(
            sync_timeout(&Some(vec![exec(30), exec(90)])),
            SYNC_BASE + Duration::from_secs(120)
        );
    }

    #[test]
    fn resending_what_the_node_just_deferred_counts_as_nothing_new() {
        let destroy = |id: &str| Command::Destroy {
            id: id.into(),
            net_index: None,
        };
        assert!(same_batch(&[destroy("a")], &[destroy("a")]));
        assert!(!same_batch(&[destroy("a")], &[destroy("b")]));
        assert!(!same_batch(&[destroy("a")], &[destroy("a"), destroy("b")]));
    }
}
