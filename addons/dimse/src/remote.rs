//! The `dimse` protocol client for peers: C-ECHO, and send jobs as C-STORE.

use std::collections::HashSet;

use medicus_addon_sdk::{Host, async_trait};
use medicus_core::capability::index::ResourceUid;
use medicus_core::capability::{
    Endpoint, HostServices as _, JobProgress, Peer, RemoteNode, SendOutcome,
};
use medicus_core::event::Origin;
use medicus_core::uid::SopInstanceUid;
use medicus_core::{AeTitle, CallContext, Error, ErrorKind, NodeInfo, Result};
use medicus_dimse::{Instance, MAX_CONTEXTS, ScuOptions, StoreScu};
use uuid::Uuid;

/// The calling AE title when no node is involved (echo) or the node has none.
const DEFAULT_CALLING_AE: &str = "MEDICUS";

#[derive(Debug)]
pub(crate) struct Dimse {
    pub(crate) host: Host,
}

fn options(calling_ae: &str, peer: &Peer) -> ScuOptions {
    let Endpoint::Dimse {
        ae_title,
        host,
        port,
    } = &peer.endpoint;
    ScuOptions::new(calling_ae, ae_title.as_str(), format!("{host}:{port}"))
}

impl Dimse {
    /// How a stored instance is encoded.
    async fn describe(&self, context: &CallContext, uid: &SopInstanceUid) -> Result<Instance> {
        let location = self
            .host
            .locate(context, &ResourceUid::Instance(uid.clone()))
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| Error::not_found(format!("instance {uid} does not exist")))?;
        Ok(Instance {
            sop_class: location.sop_class.as_str().to_owned(),
            sop_instance: location.instance.as_str().to_owned(),
            transfer_syntax: location.transfer_syntax.as_str().to_owned(),
        })
    }
}

fn unreachable(peer: &Peer, e: &medicus_dimse::Error) -> Error {
    Error::new(ErrorKind::Unavailable, format!("peer {:?}: {e}", peer.name))
}

/// Splits `instances` into groups that each fit one association's presentation contexts.
fn batches(instances: Vec<(SopInstanceUid, Instance)>) -> Vec<Vec<(SopInstanceUid, Instance)>> {
    let mut out: Vec<Vec<(SopInstanceUid, Instance)>> = Vec::new();
    let mut contexts = HashSet::new();
    for item in instances {
        let context = (item.1.sop_class.clone(), item.1.transfer_syntax.clone());
        if out.is_empty() || (!contexts.contains(&context) && contexts.len() == MAX_CONTEXTS) {
            out.push(Vec::new());
            contexts.clear();
        }
        contexts.insert(context);
        if let Some(batch) = out.last_mut() {
            batch.push(item);
        }
    }
    out
}

#[async_trait]
impl RemoteNode for Dimse {
    async fn echo(&self, peer: &Peer) -> Result<()> {
        let status = medicus_dimse::echo(&options(DEFAULT_CALLING_AE, peer))
            .await
            .map_err(|e| unreachable(peer, &e))?;
        if status.is_success() {
            Ok(())
        } else {
            Err(Error::new(
                ErrorKind::Unavailable,
                format!(
                    "peer {:?} answered C-ECHO with {:#06x}",
                    peer.name, status.code
                ),
            ))
        }
    }

    async fn send(
        &self,
        job: Uuid,
        node: &NodeInfo,
        peer: &Peer,
        uids: &[SopInstanceUid],
    ) -> Result<SendOutcome> {
        // Reads go through the node as the job's executor; the peer sees the node's AE title.
        let context = CallContext::new(node.name.clone(), Origin::Job { job_id: job });
        let calling_ae = node
            .ae_title
            .as_ref()
            .map_or(DEFAULT_CALLING_AE, AeTitle::as_str);
        let mut outcome = SendOutcome::default();
        let mut progress = JobProgress {
            total: uids.len() as u64,
            ..JobProgress::default()
        };
        let mut known = Vec::new();
        for uid in uids {
            match self.describe(&context, uid).await {
                Ok(instance) => known.push((uid.clone(), instance)),
                Err(e) if e.kind == ErrorKind::NotFound => outcome.failed.push(uid.clone()),
                Err(e) => return Err(e),
            }
        }
        let options = options(calling_ae, peer);
        let mut connected = false;
        let mut batches = batches(known).into_iter();
        while let Some(batch) = batches.next() {
            let mut scu = match StoreScu::connect(&options, batch.iter().map(|(_, i)| i)).await {
                Ok(scu) => scu,
                // The peer answered but takes none of these instances as stored.
                Err(e) if e.is_no_accepted_contexts() => {
                    tracing::warn!(peer = %peer.name, "peer accepts none of the batch's SOP classes and transfer syntaxes");
                    outcome.failed.extend(batch.into_iter().map(|(uid, _)| uid));
                    continue;
                }
                // Nothing sent yet: the peer is unreachable, retry the whole job later.
                Err(e) if !connected => return Err(unreachable(peer, &e)),
                Err(e) => {
                    tracing::warn!(peer = %peer.name, error = %e, "cannot reconnect to the peer");
                    outcome.failed.extend(batch.into_iter().map(|(uid, _)| uid));
                    outcome.failed.extend(batches.flatten().map(|(uid, _)| uid));
                    return Ok(outcome);
                }
            };
            connected = true;
            let mut items = batch.into_iter();
            while let Some((uid, instance)) = items.next() {
                let file = self.host.open_instance(&context, &uid).await?;
                match scu.store(&instance, file, None).await {
                    Ok(status) if status.is_success() => outcome.completed += 1,
                    Ok(status) if status.is_warning() => outcome.warning += 1,
                    Ok(status) => {
                        tracing::warn!(peer = %peer.name, instance = %uid, status = format_args!("{:#06x}", status.code), comment = status.comment.as_deref().unwrap_or(""), "peer did not store the instance");
                        outcome.failed.push(uid);
                    }
                    Err(e) => {
                        // The association broke: this and the remaining instances are retried.
                        tracing::warn!(peer = %peer.name, error = %e, "association to the peer failed");
                        outcome.failed.push(uid);
                        outcome.failed.extend(items.map(|(uid, _)| uid));
                        outcome.failed.extend(batches.flatten().map(|(uid, _)| uid));
                        return Ok(outcome);
                    }
                }
                progress.completed = outcome.completed;
                progress.warning = outcome.warning;
                progress.failed = outcome.failed.len() as u64;
                if !self.host.job_progress(job, progress).await? {
                    tracing::info!(%job, "send cancelled");
                    let _ = scu.abort().await;
                    return Ok(outcome);
                }
            }
            let _ = scu.release().await;
        }
        Ok(outcome)
    }
}
