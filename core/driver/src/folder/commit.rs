// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;

use koidrive_engine::commit::{Base, Failure, Halt as CommitHalt};
use koidrive_net::{CallError, Cause, Event, NotResent, ServerError};
use koidrive_proto::v1::error::Code;
use koidrive_proto::v1::{Commit, CommitResult, Error as ProtoError, op, op_result};

use crate::clock;
use crate::disk::Stop;
use crate::engine::Engine;
use crate::failures;
use crate::policy;
use crate::progress::Credit;
use crate::status::{WaitKind, describe};
use crate::timing::Time;
use koidrive_engine::question::{RecoveryCause, RecoveryTrigger};
use koidrive_engine::tree::NodeId;

use super::{Call, Flight, Folder, Next, NotTaken, Retrying, Stream, Work};

impl Folder {
    /// `Session::commit` returned. A result is acknowledged once R has reached it. A refusal goes to the engine,
    /// which says whether the same commit is sent again, its changes are planned again, or committing stops. A
    /// commit the session did not send again because the server counts fewer commits than its slot accounts for
    /// waits for the `Welcome` that reported the count, which drops it ([`Folder::connected`]).
    pub fn sent(&mut self, result: Result<CommitResult, CallError>) -> Result<Vec<Work>, Stop> {
        let mut work = Vec::new();
        let Flight::Sending(commit) = std::mem::replace(&mut self.flight, Flight::Idle) else {
            return Ok(work);
        };
        match result {
            Ok(result) => {
                crate::volume::commit_sent();
                self.failures = 0;
                self.commit_ended();
                self.result(&commit.commit_id, result)?;
            }
            Err(CallError::Server(refusal)) => {
                self.refused(commit, NotTaken::Refused(refusal), &mut work)?
            }
            Err(CallError::Stopped(_)) => {
                self.flight = Flight::Unknown;
                return Ok(work);
            }
            Err(CallError::NotResent(NotResent::DeviceSeqMismatch { slot, welcome }))
                if welcome + 1 < slot =>
            {
                self.flight = Flight::Unknown;
                return Ok(work);
            }
            Err(CallError::NotResent(why)) => {
                tracing::warn!(space_id = self.link.space_id, %why, "a commit was not sent again after reconnecting");
                let trigger = match why {
                    NotResent::HistoryChanged { .. } => RecoveryTrigger::SpaceRewound,
                    _ => RecoveryTrigger::StoreRolledBack,
                };
                self.recover(RecoveryCause::of(trigger), &mut work)?;
                return Ok(work);
            }
            Err(CallError::TooLarge { bytes }) => {
                self.refused(commit, NotTaken::TooLarge { bytes }, &mut work)?
            }
            Err(error @ CallError::UnexpectedResult) => {
                return Err(Stop::Protocol(format!("sending a commit failed: {error}")));
            }
        }
        self.drive(&mut work)?;
        Ok(work)
    }

    /// A wait is over: upload or send the commit again, or build the next one.
    pub fn resend(&mut self) -> Result<Vec<Work>, Stop> {
        let now = clock::system();
        for call in [Call::Upload, Call::Commit] {
            if let Some(retrying) = self.retrying.get_mut(&call) {
                retrying.at = retrying.at.min(now);
            }
        }
        let mut work = Vec::new();
        match std::mem::replace(&mut self.flight, Flight::Idle) {
            Flight::Waiting(Next::Send(commit)) => {
                self.flight = Flight::Sending(commit.clone());
                work.push(Work::Send(commit));
            }
            Flight::Waiting(Next::Upload) => {
                if let Some(commit) = self.in_flight() {
                    self.start(commit, &mut work);
                }
            }
            Flight::Waiting(Next::Build) => {}
            other => self.flight = other,
        }
        self.drive(&mut work)?;
        Ok(work)
    }

    /// The commit in flight went through, or is not sent again as it is: the waits of its upload and its send
    /// leave the status.
    pub(super) fn commit_ended(&mut self) {
        self.retrying.remove(&Call::Upload);
        self.retrying.remove(&Call::Commit);
    }

    /// Records that a call of kind `call` failed with `why` and is sent again after `wait`.
    pub(super) fn retry_after(
        &mut self,
        call: Call,
        why: String,
        wait: Duration,
        kind: WaitKind,
        unreachable: Option<Cause>,
    ) {
        let since =
            (self.retrying.get(&call)).map_or_else(clock::instant, |retrying| retrying.since);
        let at = clock::system() + wait;
        let retrying = Retrying {
            since,
            why,
            wait,
            at,
            kind,
            unreachable,
        };
        self.retrying.insert(call, retrying);
    }

    /// The result of this device's commit `commit_id`, from the call or from the change stream, whichever comes
    /// first (design §2.7 and §4.4). Its records are applied when it is the commit after the cursor, and skipped
    /// when R already holds it. The engine keeps the result once, and acknowledges it when R has reached its seq.
    /// The second delivery finds no commit waiting for a result and changes nothing.
    pub(super) fn result(&mut self, commit_id: &str, result: CommitResult) -> Result<(), Stop> {
        let ours = (self.engine.commits.in_flight())
            .is_some_and(|flight| flight.commit.commit_id == commit_id && flight.result.is_none());
        let flight = self.engine.commits.in_flight().filter(|_| ours);
        let next = match &self.stream {
            Stream::Replaying(staging) => result.seq == staging.last_seq() + 1,
            _ => self
                .engine
                .cursor
                .as_ref()
                .is_some_and(|cursor| result.seq == cursor.last_seq + 1),
        };
        if next {
            self.timing.forget(&result.changes);
        }
        if let Some(flight) = flight {
            self.timing.uploaded(&flight.commit, &result);
        }
        if let Some(refused) = flight.map(|flight| not_applied(&flight.commit, &result)) {
            let problems = self.engine.commits.problems();
            self.refused_nodes.retain(|id, _| problems.contains(id));
            for (node, why) in refused {
                tracing::info!(
                    space_id = self.link.space_id,
                    node,
                    path = ?self.item_path(&NodeId::from(&node)),
                    why,
                    "the server did not apply a change"
                );
                self.refused_nodes.insert(NodeId::from(node), why);
            }
        }
        let receive = |engine: &mut Engine, result: CommitResult| {
            (engine.commits.received(result)).map_err(|error| {
                Stop::Protocol(format!("the result of commit {commit_id}: {error:?}"))
            })
        };
        let at = match result.committed_at_ms {
            0 => Base::from(result.seq),
            committed_at_ms => Base {
                seq: result.seq,
                commit_id: commit_id.to_owned(),
                committed_at_ms,
            },
        };
        if let Stream::Replaying(staging) = &mut self.stream {
            if result.seq == staging.last_seq() + 1 {
                staging.replay_commit(at, &result.changes)?;
            } else {
                staging.remember(result.seq, &result.changes);
            }
            if ours {
                self.step(|engine, _| receive(engine, result))?;
            }
            return self.publish_remote();
        }
        self.remote_nodes = self.step(|engine, tx| {
            let next = (engine.cursor.as_ref()).is_some_and(|c| result.seq == c.last_seq + 1);
            if next {
                engine.apply_commit(tx, at.clone(), &result.changes)?;
            }
            if ours {
                receive(engine, result)?;
            }
            if let Some(cursor) = &engine.cursor {
                engine
                    .commits
                    .acknowledge(&mut engine.trees, cursor.last_seq);
            }
            Ok(tx.remote_node_count()?)
        })?;
        Ok(())
    }

    fn refused(
        &mut self,
        commit: Commit,
        not_taken: NotTaken,
        work: &mut Vec<Work>,
    ) -> Result<(), Stop> {
        let refusal = &match &not_taken {
            NotTaken::Refused(refusal) => refusal.clone(),
            NotTaken::TooLarge { bytes } => ServerError(ProtoError {
                code: Code::LimitExceeded as i32,
                message: format!("the commit is {bytes} bytes, more than a message may hold"),
                ..ProtoError::default()
            }),
        };
        let before = self.engine.commits.problems().clone();
        let failure = self.step(|engine, tx| {
            let in_flight = engine.commits.edited_in_flight();
            let failure = (engine.commits.failed(&mut engine.trees, &refusal.0))
                .map_err(|error| Stop::Protocol(format!("a refused commit: {error:?}")))?;
            if matches!(failure, Failure::Resync) {
                engine.server_behind(tx, in_flight)?;
            }
            Ok(failure)
        })?;
        let described = policy::describe(refusal);
        tracing::info!(
            space_id = self.link.space_id,
            refusal = described,
            ?failure,
            "a commit was refused"
        );
        let (counted, node_counted, why) = match not_taken {
            NotTaken::Refused(_) => {
                let code = failures::refusal(refusal);
                (
                    failures::Failure::Refused(code),
                    failures::Failure::ProblemNode(code),
                    format!("the server refused it: {described}"),
                )
            }
            NotTaken::TooLarge { .. } => (
                failures::Failure::TooLarge(failures::TooLarge::Commit),
                failures::Failure::TooLarge(failures::TooLarge::ProblemNode),
                format!("it is too large to send: {}", refusal.message()),
            ),
        };
        failures::failed(counted);
        for id in (self.engine.commits.problems().iter()).filter(|id| !before.contains(*id)) {
            failures::failed(node_counted);
            self.refused_nodes.insert(id.clone(), why.clone());
        }
        match failure {
            Failure::Resend if refusal.code() == Code::MissingChunks => {
                if let Some(commit) = self.in_flight() {
                    self.start(commit, work);
                }
            }
            Failure::Resend => {
                let wait =
                    (refusal.retry_after()).unwrap_or_else(|| self.backoff.delay(self.failures));
                self.failures += 1;
                self.retry_after(
                    Call::Commit,
                    format!("the server did not take a commit: {described}"),
                    wait,
                    policy::wait_kind(Some(refusal)),
                    None,
                );
                self.flight = Flight::Waiting(Next::Send(commit));
                work.push(Work::ResendAfter(wait));
            }
            Failure::Abandoned | Failure::Split | Failure::Halted(CommitHalt::ReadOnly) => {
                self.commit_ended();
            }
            Failure::Resync => {
                self.commit_ended();
                self.list_again(work);
            }
            Failure::Halted(CommitHalt::Recovery { cause, .. }) => {
                tracing::warn!(
                    space_id = self.link.space_id,
                    described,
                    why = describe(&cause),
                    "a commit was refused; recovering through a new connection"
                );
                self.flight = Flight::Idle;
                self.stream = Stream::Connecting;
                work.push(Work::Reconnect);
            }
            Failure::Halted(CommitHalt::Policy { code }) => {
                return Err(Stop::Policy {
                    problem: format!(
                        "the server refused this device's changes: {described}{}",
                        lift_hint(code)
                    ),
                    needs_owner: code == Code::ConfirmationRequired as i32,
                });
            }
        }
        Ok(())
    }

    /// Starts the upload of what `commit` adds, or its send when it adds no content.
    pub(super) fn start(&mut self, commit: Commit, work: &mut Vec<Work>) {
        let mut upload = self.disk.upload(&commit, &self.engine);
        upload.credit = Credit::upload(&self.progress, &commit, |node| self.engine.path_of(node));
        if self.timing.is_on() {
            upload.timing_bytes = self.timing.uploading(&commit, Time::now());
        }
        if upload.chunks.is_empty() {
            self.sent_commit = Some(commit.commit_id.clone());
            self.flight = Flight::Sending(commit.clone());
            work.push(Work::Send(commit));
        } else {
            self.flight = Flight::Uploading(upload.sources());
            work.push(Work::Upload(upload));
        }
    }
}

/// What lifts a halt for a refusal with `code` (RFC 0006 section 1, point 5, and section 10).
pub(super) fn lift_hint(code: i32) -> &'static str {
    if code == Code::ConfirmationRequired as i32 {
        "; the space's owner must confirm them, which Koidrive cannot ask for yet"
    } else {
        "; once the cause is fixed, run koi folder retry <folder> --resend to send them again, or --replan to plan them \
         again"
    }
}

/// The node of each op of `commit` that `result` says the server did not apply, with why. `Restore` and
/// `MarkGitTracked` ops are left out: their node never stays out of commits.
fn not_applied(commit: &Commit, result: &CommitResult) -> Vec<(String, String)> {
    use op_result::Code as Outcome;
    (commit.ops.iter().zip(&result.results))
        .filter_map(|(sent, outcome)| {
            let code = Outcome::try_from(outcome.code).unwrap_or(Outcome::Unspecified);
            if matches!(
                code,
                Outcome::Applied
                    | Outcome::AppliedRenamed
                    | Outcome::ConflictCopy
                    | Outcome::MergedInto
            ) {
                return None;
            }
            let node = match sent.op.as_ref()? {
                op::Op::CreateDir(op) => &op.node_id,
                op::Op::CreateFile(op) => &op.node_id,
                op::Op::UpdateContent(op) => &op.node_id,
                op::Op::Move(op) => &op.node_id,
                op::Op::Delete(op) => &op.node_id,
                op::Op::Restore(_) | op::Op::MarkGitTracked(_) => return None,
            };
            let why = match outcome.reject_reason.as_str() {
                "" => format!("the server did not apply it: {}", code.as_str_name()),
                reason => format!("the server rejected it: {reason}"),
            };
            Some((node.clone(), why))
        })
        .collect()
}

/// A finished `Session::commit`, after the session events `queued` before it. The session emits a connection's
/// `Connected` before it answers a commit on that connection, so the folder takes the `Welcome` before the result, and
/// the server's count in it is never older than a commit the folder acknowledged (design §2.8).
pub(super) fn sent_in_order(
    folder: &mut Folder,
    queued: impl IntoIterator<Item = Event>,
    result: Result<CommitResult, CallError>,
) -> Result<Vec<Work>, Stop> {
    let mut work = Vec::new();
    for event in queued {
        work.extend(folder.event(event)?);
    }
    work.extend(folder.sent(result)?);
    Ok(work)
}
