// SPDX-License-Identifier: Apache-2.0

use std::io;
use std::path::PathBuf;

use koidrive_engine::effects::Effect as EngineEffect;
use koidrive_fs::Unresolved;
use koidrive_net::{CallError, ChunkError};
use koidrive_proto::v1::{Commit, UploadReceipt};

use crate::clock;
use crate::disk::Stop;
use crate::failures;
use crate::policy::{self, Disposition};
use crate::progress::Credit;
use crate::timing::Time;

use super::{
    BUILD_AGAIN, Call, Denied, Flight, Folder, LEFT_OUT_AFTER, LeftOut, Next, PERMISSION_RETRY,
    Unreadable, Work,
};

impl Folder {
    pub(super) fn allowed(&self, node: &str) -> bool {
        (self.engine.effects.blocked().get(node))
            .is_none_or(|effect| self.disk.refuses(effect).is_none())
    }

    /// A [`Work::RetryAfter`] ended. Every node whose effect failed is unblocked, so the engine plans its effect again
    /// even when it is the one that failed: the disk may have changed in a way the observer does not report, such as
    /// a name that was taken for a moment. A node that fails again waits longer the next time. A node the system
    /// refused stays blocked while its target still refuses the change. A node whose last effect was stuck stays
    /// blocked, since no wait changes what keeps it.
    pub fn retry(&mut self) -> Result<Vec<Work>, Stop> {
        self.retry_at = None;
        let now = clock::instant();
        let (timed, refused): (Vec<String>, Vec<String>) = (self.engine.effects.blocked().keys())
            .filter(|node| !self.stuck_effects.contains(*node))
            .filter(|node| (self.denied.get(*node)).is_none_or(|denied| denied.due <= now))
            .cloned()
            .partition(|node| !self.denied.contains_key(node) || self.allowed(node));
        for node in &timed {
            self.denied.remove(node);
        }
        for node in refused {
            let (failures, _) = self.effect_failures.entry(node.clone()).or_default();
            let wait = PERMISSION_RETRY.delay(*failures);
            *failures += 1;
            let due = now + wait;
            self.denied.insert(
                node,
                Denied {
                    due,
                    foreseen: true,
                },
            );
        }
        let problems = self.engine.commits.problems();
        let due: Vec<String> = (self.unreadable.iter())
            .filter(|(node, unreadable)| {
                problems.contains(*node)
                    && (unreadable.left_out.as_ref()).is_some_and(|left_out| left_out.due <= now)
            })
            .map(|(node, _)| node.clone())
            .collect();
        if !timed.is_empty() {
            self.step(|engine, _| {
                for node in &timed {
                    engine.effects.unblock(node);
                }
                Ok(())
            })?;
        }
        self.let_back_in(&due)?;
        let mut work = Vec::new();
        let problems = self.engine.commits.problems();
        let blocked = self.engine.effects.blocked();
        let next = (self.unreadable.iter())
            .filter(|(node, _)| problems.contains(*node))
            .filter_map(|(_, unreadable)| unreadable.left_out.as_ref())
            .map(|left_out| left_out.due)
            .chain(
                (self.denied.iter())
                    .filter(|(node, _)| blocked.contains_key(*node))
                    .map(|(_, denied)| denied.due),
            )
            .min();
        if let Some(next) = next {
            self.retry_at = Some(next);
            work.push(Work::RetryAfter(next.saturating_duration_since(now)));
        }
        self.drive(&mut work)?;
        Ok(work)
    }

    pub(super) fn let_back_in(&mut self, nodes: &[String]) -> Result<(), Stop> {
        if nodes.is_empty() {
            return Ok(());
        }
        self.step(|engine, _| {
            for node in nodes {
                engine.commits.clear_problem(node);
            }
            Ok(())
        })?;
        for node in nodes {
            if let Some(unreadable) = self.unreadable.get_mut(node) {
                unreadable.left_out = None;
            }
        }
        Ok(())
    }

    /// The upload of the content a commit adds ended. With its receipts the commit is sent. A file that changed
    /// since it was chunked, or that can no longer be read, abandons the commit, which the server holds nothing
    /// of (design §4.2).
    pub fn uploaded(
        &mut self,
        receipts: Result<Vec<UploadReceipt>, ChunkError>,
    ) -> Result<Vec<Work>, Stop> {
        let mut work = Vec::new();
        let (sources, reused) = match std::mem::replace(&mut self.flight, Flight::Idle) {
            Flight::Uploading(sources, reused) => (sources, reused),
            other => {
                self.flight = other;
                return Ok(work);
            }
        };
        let error = match receipts {
            Ok(mut receipts) => {
                receipts.extend(reused);
                self.failures = 0;
                self.retrying.remove(&Call::Upload);
                for (node, _) in sources.values() {
                    self.unreadable.remove(node);
                }
                if let Some(flight) = self.engine.commits.in_flight() {
                    let commit = Commit {
                        receipts,
                        ..flight.commit.clone()
                    };
                    self.sent_commit = Some(commit.commit_id.clone());
                    self.flight = Flight::Sending(commit.clone());
                    work.push(Work::Send(commit));
                    self.upload_ahead(&mut work);
                }
                return Ok(work);
            }
            Err(error) => error,
        };
        failures::transfer_failed(&error, failures::Failure::Upload);
        let disposition = match &error {
            ChunkError::SourceChanged { hash } | ChunkError::Io { hash, .. } => {
                let (node, path) = sources.get(hash).cloned().unwrap_or_default();
                self.unreadable(node, path, &error, &mut work)?;
                return Ok(work);
            }
            ChunkError::Call(CallError::Stopped(_)) => {
                self.flight = Flight::Uploading(sources, reused);
                return Ok(work);
            }
            ChunkError::Call(call) => policy::after_call_error(call),
            ChunkError::Refused {
                error: Some(refusal),
                ..
            } => policy::after_refusal(refusal),
            ChunkError::Refused { .. } | ChunkError::Unavailable { .. } => Disposition::Backoff,
            ChunkError::NoTarget { .. } | ChunkError::Corrupt { .. } => Disposition::Halt,
        };
        let wait = match disposition {
            Disposition::RetryAfter(wait) => wait,
            Disposition::Backoff => self.backoff.delay(self.failures),
            Disposition::Halt => {
                let why = format!("uploading a file's content failed: {error}");
                return Err(match &error {
                    ChunkError::Call(call) => Stop::after_call(call, why),
                    ChunkError::Refused { .. } => Stop::Refused(why),
                    _ => Stop::Protocol(why),
                });
            }
        };
        tracing::info!(space_id = self.link.space_id, %error, ?wait, "uploading again later");
        let kind = policy::wait_kind(match &error {
            ChunkError::Refused {
                error: Some(refusal),
                ..
            }
            | ChunkError::Call(CallError::Server(refusal)) => Some(refusal),
            _ => None,
        });
        self.retry_after(
            Call::Upload,
            format!("uploading a file's content failed: {error}"),
            wait,
            kind,
            policy::unreachable(&error),
        );
        self.failures += 1;
        self.flight = Flight::Waiting(Next::Upload);
        work.push(Work::ResendAfter(wait));
        Ok(work)
    }

    fn unreadable(
        &mut self,
        node: String,
        path: Option<PathBuf>,
        error: &ChunkError,
        work: &mut Vec<Work>,
    ) -> Result<(), Stop> {
        let unresolved = match error {
            ChunkError::Io { source, .. } => {
                (source.get_ref()).and_then(|inner| inner.downcast_ref::<Unresolved>())
            }
            _ => None,
        };
        let (changed, scope) = match (error, unresolved) {
            (_, Some(Unresolved::RootWithdrawn(_))) => {
                return Err(Stop::Withdrawn(self.link.root.clone()));
            }
            (
                _,
                Some(
                    Unresolved::Missing(folder)
                    | Unresolved::NotAFolder(folder)
                    | Unresolved::Replaced { path: folder, .. },
                ),
            ) => (true, Some(folder.clone())),
            (ChunkError::Io { source, .. }, _) => (
                matches!(
                    source.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::UnexpectedEof
                ),
                path.clone(),
            ),
            _ => (true, path.clone()),
        };
        let failures = (self.unreadable.get(&node)).map_or(0, |unreadable| unreadable.failures) + 1;
        let why = unreadable_why(error);
        tracing::info!(
            space_id = self.link.space_id,
            path = path.as_deref().map(|path| koidrive_fs::quoted(path).to_string()),
            %error,
            failures,
            "a file could not be read for its upload"
        );
        self.disk
            .rescan(&self.engine, vec![scope.unwrap_or_default()]);
        self.commit_ended();
        if changed && failures < LEFT_OUT_AFTER {
            self.step(|engine, _| Ok(engine.commits.abandon()))?;
            self.unreadable.insert(
                node,
                Unreadable {
                    failures,
                    why,
                    left_out: None,
                },
            );
            self.flight = Flight::Waiting(Next::Build);
            work.push(Work::ResendAfter(BUILD_AGAIN));
            return Ok(());
        }
        let wait = PERMISSION_RETRY.delay(failures.saturating_sub(LEFT_OUT_AFTER));
        self.step(|engine, _| Ok(engine.commits.leave_out(&node)))?;
        let at = clock::instant() + wait;
        let seen = self.engine.trees.local().get(&node).cloned();
        self.unreadable.insert(
            node,
            Unreadable {
                failures,
                why,
                left_out: Some(LeftOut {
                    at: clock::system() + wait,
                    due: at,
                    seen,
                }),
            },
        );
        if self.retry_at.is_none_or(|out| at < out) {
            self.retry_at = Some(at);
            work.push(Work::RetryAfter(wait));
        }
        self.drive(work)
    }

    /// Plans the next batch of effects, if there is one, and asks for its staging.
    pub(super) fn plan_effect(&mut self, work: &mut Vec<Work>) -> Result<bool, Stop> {
        let Some(mut pending) = self.disk.next(&mut self.engine, &mut self.store)? else {
            return Ok(false);
        };
        let remote = self.engine.trees.remote();
        let writes = (self.engine.effects.in_flight().iter())
            .filter(|record| matches!(record.effect, EngineEffect::WriteFile { .. }))
            .filter_map(|record| Some((record.node.as_str(), remote.path(&record.node)?)));
        pending.credit = Credit::download(&self.progress, writes);
        if self.timing.is_on() {
            let at = Time::now();
            for record in self.engine.effects.in_flight() {
                let remote = self.engine.trees.remote().get(&record.node);
                let bytes = self.timing.planned(record, remote, at);
                pending.timing_bytes = pending.timing_bytes.take().or(bytes);
            }
        }
        work.push(Work::Stage(pending));
        Ok(true)
    }
}

fn unreadable_why(error: &ChunkError) -> String {
    let ChunkError::Io { source, .. } = error else {
        return "it keeps changing while it is uploaded".into();
    };
    match source.kind() {
        io::ErrorKind::PermissionDenied => "Koidrive is not allowed to read it".into(),
        _ => source.to_string(),
    }
}
