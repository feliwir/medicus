//! The job engine (PLAN §3.5): persisted outbound jobs, executed by the peer protocol's
//! `RemoteNode`, retried with exponential backoff.
//!
//! Jobs live in the index, so they survive restarts: jobs left `running` by a crash are queued
//! again at start-up. A send job that partly fails retries only the instances that failed.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use medicus_core::capability::index::ResourceUid;
use medicus_core::capability::{Job, JobKind, JobProgress, JobState, Peer, RemoteNode};
use medicus_core::event::{Event, EventKind, Origin};
use medicus_core::uid::SopInstanceUid;
use medicus_core::{Error, ErrorKind, Partition, Result};
use serde_json::json;
use time::OffsetDateTime;
use tokio::sync::Notify;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::access::Directory;
use crate::archive::Archive;
use crate::config::JobsConfig;

/// How often due retries are looked for when nothing else wakes the scheduler.
const POLL: Duration = Duration::from_secs(1);
/// Jobs read per page when recovering.
const PAGE: u32 = 100;

pub(crate) struct Jobs {
    archive: Arc<Archive>,
    directory: Arc<Directory>,
    peers: BTreeMap<String, Peer>,
    nodes: BTreeMap<String, Arc<dyn RemoteNode>>,
    cfg: JobsConfig,
    /// Running jobs whose cancellation was requested.
    cancel: Mutex<HashSet<Uuid>>,
    wake: Notify,
}

impl std::fmt::Debug for Jobs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Jobs")
            .field("peers", &self.peers.keys().collect::<Vec<_>>())
            .field("protocols", &self.nodes.keys().collect::<Vec<_>>())
            .field("cfg", &self.cfg)
            .finish_non_exhaustive()
    }
}

fn protocol(peer: &Peer) -> &'static str {
    match peer.endpoint {
        medicus_core::capability::Endpoint::Dimse { .. } => "dimse",
    }
}

impl Jobs {
    pub(crate) fn new(
        archive: Arc<Archive>,
        directory: Arc<Directory>,
        peers: impl IntoIterator<Item = Peer>,
        nodes: BTreeMap<String, Arc<dyn RemoteNode>>,
        cfg: JobsConfig,
    ) -> Self {
        Self {
            archive,
            directory,
            peers: peers.into_iter().map(|p| (p.name.clone(), p)).collect(),
            nodes,
            cfg,
            cancel: Mutex::new(HashSet::new()),
            wake: Notify::new(),
        }
    }

    pub(crate) fn peers(&self) -> impl Iterator<Item = &Peer> {
        self.peers.values()
    }

    /// The peer named `name` and the client for its protocol.
    fn resolve(&self, name: &str) -> Result<(&Peer, &Arc<dyn RemoteNode>)> {
        let peer = self
            .peers
            .get(name)
            .ok_or_else(|| Error::not_found(format!("peer {name:?} is not configured")))?;
        let node = self.nodes.get(protocol(peer)).ok_or_else(|| {
            Error::unsupported(format!(
                "no addon provides the {} protocol of peer {name:?}",
                protocol(peer)
            ))
        })?;
        Ok((peer, node))
    }

    pub(crate) async fn echo(&self, name: &str) -> Result<()> {
        let (peer, node) = self.resolve(name)?;
        node.echo(peer).await
    }

    /// Queues a job; it runs as soon as a slot is free.
    /// Queues a job for data of `partition`, submitted through `node`.
    pub(crate) async fn submit(
        &self,
        partition: &Partition,
        node: &str,
        kind: JobKind,
    ) -> Result<Job> {
        match &kind {
            JobKind::Send { peer, resources } => {
                self.resolve(peer)?;
                if resources.is_empty() {
                    return Err(Error::invalid_argument("nothing to send"));
                }
            }
        }
        let job = Job::new(
            partition.clone(),
            node.to_owned(),
            kind,
            self.cfg.max_attempts,
        );
        self.archive.index.put_job(&job).await?;
        tracing::info!(job = %job.id, "job queued");
        self.wake.notify_one();
        Ok(job)
    }

    pub(crate) async fn get(&self, id: Uuid) -> Result<Job> {
        self.archive
            .index
            .get_job(id)
            .await?
            .ok_or_else(|| Error::not_found(format!("job {id} does not exist")))
    }

    pub(crate) async fn list(
        &self,
        state: Option<JobState>,
        limit: u32,
        offset: u64,
    ) -> Result<Vec<Job>> {
        self.archive.index.list_jobs(state, limit, offset).await
    }

    /// Cancels a job: a queued job at once, a running one when its executor next reports
    /// progress.
    pub(crate) async fn cancel(&self, id: Uuid) -> Result<Job> {
        let mut job = self.get(id).await?;
        match job.state {
            JobState::Queued => {
                job.state = JobState::Cancelled;
                job.updated_at = OffsetDateTime::now_utc();
                self.archive.index.put_job(&job).await?;
                self.publish(&job, EventKind::JobFinished, json!({ "state": job.state }))
                    .await;
            }
            JobState::Running => {
                self.cancel_requests().insert(id);
            }
            _ => {}
        }
        Ok(job)
    }

    fn cancel_requests(&self) -> std::sync::MutexGuard<'_, HashSet<Uuid>> {
        self.cancel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn cancelled(&self, id: Uuid) -> bool {
        self.cancel_requests().contains(&id)
    }

    /// Records the progress of a running job; `false` once it was cancelled.
    pub(crate) async fn progress(&self, id: Uuid, progress: JobProgress) -> Result<bool> {
        let mut job = self.get(id).await?;
        if job.state != JobState::Running {
            return Ok(false);
        }
        job.progress = progress;
        job.updated_at = OffsetDateTime::now_utc();
        self.archive.index.put_job(&job).await?;
        Ok(!self.cancelled(id))
    }

    async fn publish(&self, job: &Job, kind: EventKind, detail: serde_json::Value) {
        let event = Event::new(kind, Origin::Job { job_id: job.id }).with_detail(detail);
        match self.archive.index.append_event(&event).await {
            Ok(seq) => self.archive.events_committed(seq),
            Err(e) => tracing::warn!(job = %job.id, error = %e, "cannot publish job event"),
        }
    }

    /// Runs queued jobs until `shutdown`; running jobs are interrupted and queued again at the
    /// next start.
    pub(crate) async fn run(self: Arc<Self>, shutdown: CancellationToken) {
        if let Err(e) = self.recover().await {
            tracing::error!(error = %e, "cannot requeue interrupted jobs");
        }
        let mut running = JoinSet::new();
        loop {
            while running.len() < self.cfg.concurrency.max(1) {
                match self
                    .archive
                    .index
                    .claim_job(OffsetDateTime::now_utc())
                    .await
                {
                    Ok(Some(job)) => {
                        let jobs = self.clone();
                        running.spawn(async move { jobs.execute(job).await });
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!(error = %e, "cannot claim a job");
                        break;
                    }
                }
            }
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = self.wake.notified() => {}
                () = tokio::time::sleep(POLL) => {}
                Some(_) = running.join_next(), if !running.is_empty() => {}
            }
        }
        running.shutdown().await;
    }

    /// Queues jobs a previous process left running.
    async fn recover(&self) -> Result<()> {
        loop {
            let interrupted = self
                .archive
                .index
                .list_jobs(Some(JobState::Running), PAGE, 0)
                .await?;
            if interrupted.is_empty() {
                return Ok(());
            }
            for mut job in interrupted {
                tracing::info!(job = %job.id, "requeueing job interrupted by a restart");
                job.state = JobState::Queued;
                job.updated_at = OffsetDateTime::now_utc();
                self.archive.index.put_job(&job).await?;
            }
        }
    }

    /// Runs one attempt of a claimed job and records the outcome.
    async fn execute(&self, mut job: Job) {
        tracing::info!(job = %job.id, attempt = job.attempts, "job started");
        self.publish(
            &job,
            EventKind::JobStarted,
            json!({ "attempt": job.attempts }),
        )
        .await;
        let result = self.attempt(&mut job).await;
        let cancelled = self.cancel_requests().remove(&job.id);
        let now = OffsetDateTime::now_utc();
        job.updated_at = now;
        let event = match result {
            _ if cancelled => {
                job.state = JobState::Cancelled;
                Some(EventKind::JobFinished)
            }
            Ok(None) => {
                job.state = JobState::Succeeded;
                job.pending = None;
                job.error = None;
                Some(EventKind::JobFinished)
            }
            Ok(Some(e)) | Err(e) => {
                let permanent = matches!(
                    e.kind,
                    ErrorKind::NotFound | ErrorKind::InvalidArgument | ErrorKind::Unsupported
                );
                job.error = Some(e.message.clone());
                if e.kind == ErrorKind::Unavailable {
                    self.publish(
                        &job,
                        EventKind::PeerUnreachable,
                        json!({ "error": e.message }),
                    )
                    .await;
                }
                if permanent || job.attempts >= job.max_attempts {
                    job.state = JobState::Failed;
                    Some(EventKind::JobFailed)
                } else {
                    job.state = JobState::Queued;
                    job.not_before = now + self.backoff(job.attempts);
                    tracing::info!(job = %job.id, error = %e, retry_at = %job.not_before, "job attempt failed; retrying");
                    None
                }
            }
        };
        if let Err(e) = self.archive.index.put_job(&job).await {
            tracing::error!(job = %job.id, error = %e, "cannot record the job outcome");
            return;
        }
        if let Some(kind) = event {
            tracing::info!(job = %job.id, state = job.state.as_str(), "job ended");
            let detail =
                json!({ "state": job.state, "progress": job.progress, "error": job.error });
            self.publish(&job, kind, detail).await;
        }
        self.wake.notify_one();
    }

    /// One attempt. `Ok(Some(error))`: some instances failed (they are kept in `pending`).
    async fn attempt(&self, job: &mut Job) -> Result<Option<Error>> {
        let JobKind::Send { peer, resources } = &job.kind;
        let (peer, node) = self.resolve(peer)?;
        let instances = match &job.pending {
            Some(pending) => pending.clone(),
            None => self.instances(&job.partition, resources).await?,
        };
        job.progress = JobProgress {
            total: instances.len() as u64,
            ..JobProgress::default()
        };
        self.archive.index.put_job(job).await?;
        let source = self.directory.node(&job.node)?.info();
        let outcome = node.send(job.id, &source, peer, &instances).await?;
        job.progress = JobProgress {
            total: instances.len() as u64,
            completed: outcome.completed,
            failed: outcome.failed.len() as u64,
            warning: outcome.warning,
        };
        if outcome.failed.is_empty() {
            return Ok(None);
        }
        let error = Error::new(
            ErrorKind::Unavailable,
            format!(
                "{} of {} instances were not stored",
                outcome.failed.len(),
                instances.len()
            ),
        );
        job.pending = Some(outcome.failed);
        Ok(Some(error))
    }

    /// The stored instances of `resources`, without duplicates.
    async fn instances(
        &self,
        partition: &Partition,
        resources: &[ResourceUid],
    ) -> Result<Vec<SopInstanceUid>> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for resource in resources {
            for location in self.archive.index.locate(partition, resource).await? {
                if seen.insert(location.instance.clone()) {
                    out.push(location.instance);
                }
            }
        }
        if out.is_empty() {
            return Err(Error::not_found("the job's resources hold no instances"));
        }
        Ok(out)
    }

    fn backoff(&self, attempts: u32) -> time::Duration {
        let base = self.cfg.retry_base_secs.max(1);
        let secs = base.saturating_mul(1 << attempts.saturating_sub(1).min(16));
        time::Duration::seconds(i64::try_from(secs.min(3600)).unwrap_or(3600))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use medicus_core::capability::{Endpoint, SendOutcome};
    use medicus_core::uid::StudyInstanceUid;
    use medicus_testkit::dicom::InstanceBuilder;

    /// Fails each instance the first time it is sent; records every send.
    #[derive(Default)]
    struct Flaky {
        sent: Mutex<Vec<Vec<SopInstanceUid>>>,
    }

    #[async_trait::async_trait]
    impl RemoteNode for Flaky {
        async fn echo(&self, _peer: &Peer) -> Result<()> {
            Ok(())
        }

        async fn send(
            &self,
            _job: Uuid,
            _node: &medicus_core::NodeInfo,
            _peer: &Peer,
            instances: &[SopInstanceUid],
        ) -> Result<medicus_core::capability::SendOutcome> {
            let mut sent = self.sent.lock().unwrap();
            let first = sent.is_empty();
            sent.push(instances.to_vec());
            Ok(if first {
                SendOutcome {
                    completed: 1,
                    warning: 0,
                    failed: instances[1..].to_vec(),
                }
            } else {
                SendOutcome {
                    completed: instances.len() as u64,
                    ..SendOutcome::default()
                }
            })
        }
    }

    async fn engine(dir: &std::path::Path) -> (Arc<Jobs>, Arc<Flaky>) {
        let mut cfg = Config::default();
        cfg.server.data_dir = dir.to_owned();
        cfg.jobs.retry_base_secs = 1;
        let caps = crate::addons::load(&cfg, &tokio::runtime::Handle::current()).unwrap();
        let archive = Arc::new(Archive::new(&caps, &cfg.archive).await.unwrap());
        for sop in ["1.2.3.1.1", "1.2.3.1.2", "1.2.3.1.3"] {
            let file = InstanceBuilder::new("1.2.3", "1.2.3.1", sop).build();
            let origin = Origin::Internal {
                component: "test".into(),
            };
            archive
                .ingest(
                    &medicus_core::Partition::default_partition(),
                    medicus_testkit::stream(&file, 4096),
                    origin,
                )
                .await
                .unwrap();
        }
        let flaky = Arc::new(Flaky::default());
        let peer = Peer {
            name: "pacs".into(),
            endpoint: Endpoint::Dimse {
                ae_title: medicus_core::AeTitle::parse("PACS").unwrap(),
                host: "127.0.0.1".into(),
                port: 104,
            },
        };
        let nodes = BTreeMap::from([("dimse".to_owned(), flaky.clone() as Arc<dyn RemoteNode>)]);
        let directory = Arc::new(Directory::from_config(&cfg).unwrap());
        (
            Arc::new(Jobs::new(archive, directory, [peer], nodes, cfg.jobs)),
            flaky,
        )
    }

    fn send_study() -> JobKind {
        JobKind::Send {
            peer: "pacs".into(),
            resources: vec![ResourceUid::Study(
                StudyInstanceUid::parse("1.2.3").unwrap(),
            )],
        }
    }

    async fn wait(jobs: &Jobs, id: Uuid, state: JobState) -> Job {
        for _ in 0..500 {
            let job = jobs.get(id).await.unwrap();
            if job.state == state {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("job never reached {state:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn retries_only_the_failed_instances() {
        let dir = tempfile::tempdir().unwrap();
        let (jobs, flaky) = engine(dir.path()).await;
        let stop = CancellationToken::new();
        let runner = tokio::spawn(jobs.clone().run(stop.clone()));
        let job = jobs
            .submit(
                &medicus_core::Partition::default_partition(),
                crate::access::DEFAULT_NODE,
                send_study(),
            )
            .await
            .unwrap();
        let done = wait(&jobs, job.id, JobState::Succeeded).await;
        assert_eq!(done.attempts, 2);
        let sent = flaky.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].len(), 3);
        assert_eq!(sent[1], sent[0][1..], "the retry sends only what failed");
        stop.cancel();
        runner.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_peers_are_refused_and_queued_jobs_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let (jobs, _) = engine(dir.path()).await;
        let unknown = JobKind::Send {
            peer: "nowhere".into(),
            resources: vec![ResourceUid::Study(
                StudyInstanceUid::parse("1.2.3").unwrap(),
            )],
        };
        assert_eq!(
            jobs.submit(
                &medicus_core::Partition::default_partition(),
                crate::access::DEFAULT_NODE,
                unknown
            )
            .await
            .unwrap_err()
            .kind,
            ErrorKind::NotFound
        );
        // Without a running scheduler the job stays queued.
        let job = jobs
            .submit(
                &medicus_core::Partition::default_partition(),
                crate::access::DEFAULT_NODE,
                send_study(),
            )
            .await
            .unwrap();
        assert_eq!(
            jobs.cancel(job.id).await.unwrap().state,
            JobState::Cancelled
        );
        assert_eq!(jobs.get(job.id).await.unwrap().state, JobState::Cancelled);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn interrupted_jobs_are_requeued() {
        let dir = tempfile::tempdir().unwrap();
        let (jobs, flaky) = engine(dir.path()).await;
        let job = jobs
            .submit(
                &medicus_core::Partition::default_partition(),
                crate::access::DEFAULT_NODE,
                send_study(),
            )
            .await
            .unwrap();
        // A crash after claiming leaves the job running.
        let claimed = jobs
            .archive
            .index
            .claim_job(OffsetDateTime::now_utc())
            .await
            .unwrap();
        assert_eq!(claimed.unwrap().state, JobState::Running);
        let stop = CancellationToken::new();
        let runner = tokio::spawn(jobs.clone().run(stop.clone()));
        wait(&jobs, job.id, JobState::Succeeded).await;
        assert!(!flaky.sent.lock().unwrap().is_empty());
        stop.cancel();
        runner.await.unwrap();
    }
}
