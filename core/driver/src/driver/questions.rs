// SPDX-License-Identifier: Apache-2.0

use super::*;

const DEVICES_WAIT: Duration = Duration::from_secs(3);

const DEVICES_RELIST: Duration = Duration::from_secs(60);

/// What [`Driver::answer`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answered {
    /// The answer is recorded now.
    Now,
    /// The question had this answer already, waiting for the engine or taken by it, and nothing changed.
    Already,
}

type DeviceNames = HashMap<String, String>;

fn name_writers(questions: &mut [Question], names: &DeviceNames) -> bool {
    let mut unknown = false;
    for item in questions
        .iter_mut()
        .flat_map(|question| question.items.iter_mut())
    {
        let Some(device_id) = &item.device_id else {
            continue;
        };
        item.device_name = names.get(device_id).cloned();
        unknown |= item.device_name.is_none();
    }
    unknown
}

/// The question `stored` holds. A mass-delete question stored before questions kept the limits
/// they crossed names those its counts cross, or the deletions limit when they cross none.
pub(super) fn decode_question(space_id: &str, stored: &StoredQuestion) -> Option<Question> {
    match serde_json::from_slice::<Question>(&stored.body) {
        Ok(mut question) => {
            if let QuestionKind::MassDelete {
                deletions,
                affected_files,
                space_files,
                exceeded,
            } = &mut question.kind
                && exceeded.is_empty()
            {
                let window = Window {
                    deletions: *deletions,
                    affected_files: *affected_files,
                    space_files: *space_files,
                };
                *exceeded = window.exceeded();
                if exceeded.is_empty() {
                    exceeded.push(Limit::Deletions);
                }
            }
            if question.lineage.is_empty() {
                question.lineage = question.id.clone();
            }
            Some(question)
        }
        Err(error) => {
            tracing::warn!(space_id, question_id = stored.question_id, %error, "a stored question cannot be read");
            None
        }
    }
}

impl Driver {
    /// Unanswered questions of every linked folder, oldest first per folder. Both files of a conflict show their
    /// current version in the folder's remote tree and the name of the device that wrote it.
    /// A writer the cached device list does not name is unnamed here, and a listing of the account's devices starts in
    /// the background, at most once a minute, which marks [`Driver::watch`] changed when it names writers.
    pub fn questions(&self) -> Result<Vec<Question>, DriverError> {
        let runtime = tokio::runtime::Handle::try_current().ok();
        let (questions, listing) = self.named_questions(runtime.is_some(), true)?;
        if let (Some(runtime), Some(listing)) = (runtime, listing) {
            let inner = Arc::downgrade(&self.inner);
            let listing = runtime
                .spawn(async move {
                    let (Some(names), Some(inner)) = (listing.await, inner.upgrade()) else {
                        return;
                    };
                    inner.devices.lock().unwrap().names = names;
                    inner.changed.send_replace(());
                })
                .abort_handle();
            if let Some(earlier) = self.inner.devices.lock().unwrap().listing.replace(listing) {
                earlier.abort();
            }
        }
        Ok(questions)
    }

    /// Unanswered questions, as [`Driver::questions`] reads them, for a process that reads them once. This closes the
    /// driver and frees the profile before it lists the account's devices, for at most three seconds, when an open
    /// conflict has a writer the cached device list does not name.
    pub async fn into_questions(self) -> Result<Vec<Question>, DriverError> {
        let (mut questions, listing) = self.named_questions(true, false)?;
        drop(self);
        if let Some(listing) = listing
            && let Ok(Some(names)) = tokio::time::timeout(DEVICES_WAIT, listing).await
        {
            name_writers(&mut questions, &names);
        }
        Ok(questions)
    }

    pub(super) fn open_questions(&self) -> Result<Vec<Question>, DriverError> {
        let mut questions = Vec::new();
        for space_id in self.space_ids() {
            let open = match self.read_store(&space_id, |store| {
                store.read(|tx| {
                    let mut open = Vec::new();
                    for stored in tx.questions()? {
                        let Some(mut question) = (stored.answer.is_none())
                            .then(|| decode_question(&space_id, &stored))
                            .flatten()
                        else {
                            continue;
                        };
                        if let QuestionKind::Conflict { .. } = question.kind {
                            for item in &mut question.items {
                                item.show_current(tx.remote_current(&item.node_id)?);
                            }
                        }
                        open.push(question);
                    }
                    Ok(open)
                })
            }) {
                Err(DriverError::Store { .. }) if self.halted(&space_id) => continue,
                open => open?,
            };
            questions.extend(open.into_iter().flatten());
        }
        Ok(questions)
    }

    fn named_questions(
        &self,
        may_list: bool,
        holds_profile: bool,
    ) -> Result<
        (
            Vec<Question>,
            Option<impl Future<Output = Option<DeviceNames>> + Send + 'static>,
        ),
        DriverError,
    > {
        let mut questions = self.open_questions()?;
        let mut devices = self.inner.devices.lock().unwrap();
        let unknown = name_writers(&mut questions, &devices.names);
        let due = devices
            .listed_at
            .is_none_or(|at| at.elapsed() >= DEVICES_RELIST);
        if !(may_list && unknown && due) {
            return Ok((questions, None));
        }
        let auth = match holds_profile {
            true => self.signed_in(),
            false => self.reading_auth(),
        };
        let Ok(auth) = auth else {
            return Ok((questions, None));
        };
        devices.listed_at = Some(Instant::now());
        let listing = async move {
            match auth.api().all_devices(&auth).await {
                Ok(listed) => Some(
                    (listed.into_iter())
                        .filter(|device| !device.name.trim().is_empty())
                        .map(|device| (device.device_id, device.name))
                        .collect(),
                ),
                Err(error) => {
                    tracing::info!(%error, "listing the account's devices failed; conflicts name no writers");
                    None
                }
            }
        };
        Ok((questions, Some(listing)))
    }

    /// The question `question_id` once it has an answer, while the answer waits for the engine or after the engine took
    /// it, so that an answer given again can be read against it. None when the question is unanswered or the engine
    /// dropped it before it took an answer.
    pub fn answered_question(&self, question_id: &str) -> Result<Option<Question>, DriverError> {
        for space_id in self.space_ids() {
            let stored = match self.read_store(&space_id, |store| {
                store.read(|tx| {
                    let current = (tx.questions()?.into_iter())
                        .find(|stored| stored.question_id == question_id);
                    match current {
                        Some(stored) => Ok(stored.answer.is_some().then_some(stored)),
                        None => tx.taken_question(question_id),
                    }
                })
            }) {
                Err(DriverError::Store { .. }) if self.halted(&space_id) => continue,
                stored => stored?,
            };
            if let Some(question) =
                (stored.flatten()).and_then(|stored| decode_question(&space_id, &stored))
            {
                return Ok(Some(question));
            }
        }
        Ok(None)
    }

    /// Records the answer to a question. The folder's task takes it as the engine's `UserDecision` input, now if it
    /// runs and otherwise when it next opens the folder. The same answer again, while it waits for the task or after
    /// the engine took it, changes nothing. Another answer is refused, and so is any answer to a question the engine
    /// dropped before it took the answer, as when it asks again about a changed set.
    pub fn answer(&self, question_id: &str, answer: Answer) -> Result<Answered, DriverError> {
        let earlier = |given: &str| match given.parse::<Answer>() {
            Ok(given) if given == answer => Ok(None),
            Ok(given) => Err(DriverError::AnsweredOtherwise { given }),
            Err(_) => Err(DriverError::QuestionNotFound(question_id.to_owned())),
        };
        for space_id in self.space_ids() {
            let outcome = match self.read_store(&space_id, |store| {
                store.transaction(|tx| {
                    let stored = (tx.questions()?.into_iter())
                        .find(|stored| stored.question_id == question_id);
                    let stored = match stored {
                        None => {
                            return Ok((tx.taken_question(question_id)?)
                                .and_then(|taken| taken.answer)
                                .map(|given| earlier(&given.answer)));
                        }
                        Some(StoredQuestion {
                            answer: Some(given),
                            ..
                        }) => return Ok(Some(earlier(&given.answer))),
                        Some(stored) => stored,
                    };
                    let Some(question) = decode_question(&space_id, &stored) else {
                        return Ok(None);
                    };
                    let allowed = question.kind.answers();
                    if !allowed.contains(&answer) {
                        return Ok(Some(Err(DriverError::AnswerNotAllowed { allowed })));
                    }
                    let given = GivenAnswer {
                        answer: answer.as_str().to_owned(),
                        answered_at_ms: now_ms(),
                    };
                    tx.answer_question(question_id, &given)?;
                    let left = tx
                        .questions()?
                        .iter()
                        .filter(|q| q.answer.is_none())
                        .count();
                    Ok(Some(Ok(Some(left as u64))))
                })
            }) {
                Err(DriverError::Store { .. }) if self.halted(&space_id) => continue,
                outcome => outcome?,
            };
            match outcome.flatten() {
                None => continue,
                Some(Err(error)) => return Err(error),
                Some(Ok(None)) => return Ok(Answered::Already),
                Some(Ok(Some(left))) => {
                    if let Some(folder) = self.inner.folders.lock().unwrap().get(&space_id) {
                        folder.status.lock().unwrap().questions = left;
                        if let Some(run) = &folder.run {
                            run.requests.notify_one();
                        }
                    }
                    tracing::info!(space_id, question_id, %answer, "answered a question");
                    self.changed();
                    return Ok(Answered::Now);
                }
            }
        }
        Err(DriverError::QuestionNotFound(question_id.to_owned()))
    }

    /// Asks for a restore in a linked space: a trashed node back from the trash, or an earlier version of a file
    /// made current. Returns the restore's ID once the request is in the folder's store, where it waits for the
    /// engine, also while the driver is stopped, offline or halted. The engine sends it as a `Restore` op in its next
    /// commit, and [`Driver::restores`] tells how it ended. A request is not sent twice and cannot be taken back.
    ///
    /// A malformed ID is refused here, because the server refuses a whole commit for one. Whether the node or the
    /// version still exists is the server's answer.
    ///
    /// `seen` is the item as the person saw it. [`Driver::restores`] shows its path, and a question about the restore
    /// its kind and files, while the folder's copy of the server's tree cannot place the node, as for one deleted before
    /// the folder first synced or after a restore point the server was rewound to.
    pub fn restore(
        &self,
        space_id: &str,
        request: RestoreRequest,
        seen: Seen,
    ) -> Result<String, DriverError> {
        let asked = AskedRestore {
            path: seen.path,
            version_saved_at_ms: request.saved_at_ms(),
            kind: seen.kind.map(Kind::from),
            files: seen.files,
        };
        let intent = request
            .intent()
            .map_err(|malformed| DriverError::InvalidRestore(malformed.0))?;
        let id = restore::new_id(now_ms() as u64);
        self.read_store(space_id, |store| {
            store.transaction(|tx| {
                tx.request_restore(&id, &intent)?;
                tx.note_asked_restore(&id, &asked)
            })
        })?
        .ok_or_else(|| DriverError::SpaceNotLinked(space_id.to_owned()))?;
        tracing::info!(space_id, restore_id = %id, "recorded a restore request");
        let folders = self.inner.folders.lock().unwrap();
        if let Some(run) = folders.get(space_id).and_then(|folder| folder.run.as_ref()) {
            run.requests.notify_one();
        }
        drop(folders);
        self.changed();
        Ok(id)
    }

    /// The restores asked for in a linked space: first those that ended, in the order they ended, then those
    /// that wait, in request order. A waiting restore whose node has a problem says so, because it stays pending
    /// until the problem is cleared. The store keeps the last 1,000 that ended and drops older ones.
    pub fn restores(&self, space_id: &str) -> Result<Vec<RestoreStatus>, DriverError> {
        self.read_store(space_id, |store| {
            store.read(|tx| restore::list(tx, space_id))
        })?
        .ok_or_else(|| DriverError::SpaceNotLinked(space_id.to_owned()))
    }

    #[cfg(test)]
    pub(super) fn stored_questions(
        &self,
        space_id: &str,
    ) -> Result<Option<Vec<StoredQuestion>>, DriverError> {
        self.read_store(space_id, |store| store.transaction(|tx| tx.questions()))
    }
}
