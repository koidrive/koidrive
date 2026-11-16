// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use koidrive_driver::{self as driver, FolderState as DriverFolderState, Status};

use crate::types::{
    Account, Answer, ConflictSide, FolderPath, FolderState, HeldItem, ItemDetail, LinkedFolder,
    MassDeleteLimit, NodeKind, Notice, NoticeKind, Problem, ProblemKind, Question, QuestionKind,
    RecoveryCause, RecoveryGroup, RecoveryTrigger, SessionState, SyncProgress, SyncStatus,
    UpdateStatus,
};

pub(crate) type Seen = BTreeMap<String, (String, i64)>;

pub(crate) fn session_state(
    status: &Status,
    questions: &[driver::Question],
    started: bool,
    seen: &mut Seen,
) -> SessionState {
    let shown = match started {
        true => sync_status(status),
        false => SyncStatus::Stopped,
    };
    SessionState {
        status: shown,
        progress: match shown {
            SyncStatus::Syncing => progress(status),
            _ => None,
        },
        account: status.account.as_ref().map(account),
        folders: status.folders.iter().map(folder).collect(),
        recent_changes: Vec::new(),
        questions: questions.iter().cloned().filter_map(question).collect(),
        problems: problems(status, seen),
        notices: questions.iter().cloned().filter_map(notice).collect(),
        update: update(&status.update),
    }
}

pub(crate) fn update(update: &driver::Update) -> UpdateStatus {
    match update {
        driver::Update::Available {
            latest,
            url,
            checked_at_ms,
        } => UpdateStatus::Available {
            version: latest.clone(),
            release_url: url.clone(),
            checked_at_ms: *checked_at_ms,
        },
        driver::Update::UpToDate { checked_at_ms } => UpdateStatus::UpToDate {
            checked_at_ms: *checked_at_ms,
        },
        driver::Update::Unknown => UpdateStatus::Unknown,
        driver::Update::Off => UpdateStatus::Off,
    }
}

fn sync_status(status: &Status) -> SyncStatus {
    if status.paused {
        return SyncStatus::Paused;
    }
    if status.account.is_none() || status.folders.is_empty() {
        return SyncStatus::NeedsSetup;
    }
    if !status.running {
        return SyncStatus::Stopped;
    }
    let any = |state| status.folders.iter().any(|folder| folder.state == state);
    if any(DriverFolderState::Halted) {
        SyncStatus::Halted
    } else if any(DriverFolderState::Offline) {
        SyncStatus::Offline
    } else if any(DriverFolderState::Bootstrapping) || any(DriverFolderState::CatchingUp) {
        SyncStatus::Syncing
    } else if any(DriverFolderState::Connecting) || any(DriverFolderState::Stopped) {
        SyncStatus::Connecting
    } else {
        SyncStatus::Idle
    }
}

fn progress(status: &Status) -> Option<SyncProgress> {
    let totals = status.progress.as_ref()?;
    Some(SyncProgress {
        files_to_upload: totals.files_to_upload,
        files_to_download: totals.files_to_download,
        bytes_done: totals.bytes_done,
        bytes_total: totals.bytes_total,
        current: status.folders.iter().find_map(|folder| {
            Some(FolderPath {
                space_id: folder.space_id.clone(),
                path: folder.progress.as_ref()?.current.clone()?,
            })
        }),
    })
}

fn account(account: &driver::Account) -> Account {
    Account {
        server_url: account.server.trim_end_matches('/').to_owned(),
        user_id: account.user_id.clone(),
        user_name: account.user_name.clone().unwrap_or_default(),
        device_id: account.device_id.clone(),
        device_name: account.device_name.clone(),
    }
}

pub(crate) fn folder(folder: &driver::FolderStatus) -> LinkedFolder {
    LinkedFolder {
        space_id: folder.space_id.clone(),
        space_name: folder.space_name.clone(),
        path: folder.path.clone(),
        state: match folder.state {
            DriverFolderState::Stopped | DriverFolderState::Connecting => FolderState::Waiting,
            DriverFolderState::Offline => FolderState::Offline,
            DriverFolderState::Bootstrapping | DriverFolderState::CatchingUp => {
                FolderState::Syncing
            }
            DriverFolderState::UpToDate => FolderState::UpToDate,
            DriverFolderState::Halted => FolderState::Halted,
        },
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

fn problems(status: &Status, seen: &mut Seen) -> Vec<Problem> {
    let now = now_ms();
    let mut found: Vec<(String, String, Problem)> = Vec::new();
    for folder in &status.folders {
        let space = &folder.space_id;
        let space_id = Some(space.clone());
        if folder.state == DriverFolderState::Halted {
            let reason = folder.problem.clone().unwrap_or_default();
            found.push((
                format!("halted-{space}"),
                reason.clone(),
                Problem {
                    id: String::new(),
                    space_id: space_id.clone(),
                    path: None,
                    kind: ProblemKind::FolderHalted { reason },
                    since_ms: 0,
                },
            ));
        }
        for disk in &folder.held_disks {
            let kind = match disk.kind {
                driver::HeldDiskKind::NotConnected => ProblemKind::DiskNotConnected,
                driver::HeldDiskKind::MountedOver => ProblemKind::DiskMountedOver,
            };
            found.push((
                format!("disk-{space}-{}", disk.path),
                format!("{kind:?}"),
                Problem {
                    id: String::new(),
                    space_id: space_id.clone(),
                    path: Some(disk.path.clone()),
                    kind,
                    since_ms: 0,
                },
            ));
        }
        if status.running
            && let Some((unreachable, retry_at_ms)) = folder.unreachable()
        {
            found.push((
                format!("offline-{space}"),
                String::new(),
                Problem {
                    id: String::new(),
                    space_id: space_id.clone(),
                    path: None,
                    kind: match unreachable.no_internet {
                        true => ProblemKind::NoInternet { retry_at_ms },
                        false => ProblemKind::ServerUnreachable {
                            reason: unreachable.why.clone(),
                            retry_at_ms,
                        },
                    },
                    since_ms: 0,
                },
            ));
        }
    }
    let mut problems = Vec::new();
    let mut current = Seen::new();
    for (id, key, problem) in found {
        let since = match seen.get(&id) {
            Some((was, since)) if *was == key => *since,
            _ => now,
        };
        current.insert(id.clone(), (key, since));
        problems.push(Problem {
            id,
            since_ms: since,
            ..problem
        });
    }
    *seen = current;
    problems.sort_by_key(|problem| problem.since_ms);
    problems
}

fn mass_delete_limit(limit: driver::Limit) -> MassDeleteLimit {
    match limit {
        driver::Limit::Deletions => MassDeleteLimit::Deletions {
            limit: driver::MAX_DELETIONS,
        },
        driver::Limit::Share => MassDeleteLimit::ShareOfSpace { percent: 50 },
    }
}

fn notice(asked: driver::Question) -> Option<Notice> {
    let driver::QuestionKind::RemoteMassDelete {
        deletions,
        affected_files,
        space_files,
        exceeded,
    } = asked.kind
    else {
        return None;
    };
    Some(Notice {
        id: asked.id,
        space_id: asked.space_id,
        at_ms: asked.asked_at_ms,
        kind: NoticeKind::RemoteMassDelete {
            deletions,
            affected_files,
            space_files,
            exceeded: exceeded.into_iter().map(mass_delete_limit).collect(),
        },
    })
}

fn question(question: driver::Question) -> Option<Question> {
    let kind = match question.kind {
        driver::QuestionKind::RemoteMassDelete { .. } => return None,
        driver::QuestionKind::MassDelete {
            deletions,
            affected_files,
            space_files,
            exceeded,
        } => QuestionKind::MassDelete {
            deletions,
            affected_files,
            space_files,
            exceeded: exceeded.into_iter().map(mass_delete_limit).collect(),
        },
        driver::QuestionKind::AmbiguousDisappearance => QuestionKind::AmbiguousDisappearance,
        driver::QuestionKind::Recovery { group, .. } => QuestionKind::Recovery {
            cause: recovery_cause(question.recovery_cause),
            group: match group {
                driver::RecoveryGroup::CreatedAfterRestorePoint => {
                    RecoveryGroup::CreatedAfterRestorePoint
                }
                driver::RecoveryGroup::ChangedOnThisDevice => RecoveryGroup::ChangedOnThisDevice,
                driver::RecoveryGroup::CreatedOnThisDevice => RecoveryGroup::CreatedOnThisDevice,
                driver::RecoveryGroup::DeletedOnThisDevice => RecoveryGroup::DeletedOnThisDevice,
                driver::RecoveryGroup::RequestedBeforeReset => RecoveryGroup::RequestedBeforeReset,
            },
        },
        driver::QuestionKind::Conflict { original, copy, .. } => {
            let side = |node_id: &str| {
                let item = question.items.iter().find(|item| item.node_id == node_id)?;
                Some(ConflictSide {
                    node_id: item.node_id.clone(),
                    path: item.path.clone(),
                    size: item.size?,
                    modified_at_ms: item.modified_at_ms?,
                })
            };
            let (Some(original), Some(copy)) = (side(&original.node_id), side(&copy.node_id))
            else {
                tracing::warn!(
                    question_id = question.id,
                    "a conflict lacks the path, size or modification time of a file, so it is not shown"
                );
                return None;
            };
            QuestionKind::Conflict { original, copy }
        }
    };
    Some(Question {
        id: question.id,
        space_id: question.space_id,
        asked_at_ms: question.asked_at_ms,
        answers: kind.answers(),
        kind,
        items: question
            .items
            .into_iter()
            .map(|item| HeldItem {
                node_id: item.node_id,
                path: item.path,
                kind: match item.files {
                    1 => NodeKind::File,
                    _ => NodeKind::Folder,
                },
                files: item.files,
                detail: ItemDetail::Plain,
            })
            .collect(),
    })
}

fn recovery_cause(cause: Option<driver::RecoveryCause>) -> RecoveryCause {
    let Some(cause) = cause else {
        return RecoveryCause {
            trigger: RecoveryTrigger::Unknown,
            restore_point_ms: None,
        };
    };
    let trigger = match cause.trigger {
        driver::RecoveryTrigger::SpaceRewound => RecoveryTrigger::SpaceRewound,
        driver::RecoveryTrigger::StoreRolledBack => RecoveryTrigger::StoreRolledBack,
        driver::RecoveryTrigger::StoreOlderThanDisk => RecoveryTrigger::StoreOlderThanDisk,
        driver::RecoveryTrigger::SpaceImported => RecoveryTrigger::SpaceImported,
        driver::RecoveryTrigger::NoSyncState => RecoveryTrigger::NoSyncState,
        driver::RecoveryTrigger::Unknown => RecoveryTrigger::Unknown,
    };
    RecoveryCause {
        trigger,
        restore_point_ms: match trigger {
            RecoveryTrigger::Unknown => None,
            _ => cause.restore_point_ms,
        },
    }
}

pub(crate) fn word(answer: Answer, kind: Option<&driver::QuestionKind>) -> Option<String> {
    let conflict = match kind {
        Some(driver::QuestionKind::Conflict { original, copy, .. }) => Some((original, copy)),
        _ => None,
    };
    let engine = match answer {
        Answer::Delete => driver::Answer::Delete,
        Answer::Restore => driver::Answer::Restore,
        Answer::Keep => driver::Answer::Keep,
        Answer::Discard => driver::Answer::Discard,
        Answer::Publish => driver::Answer::Publish,
        Answer::Repeat => driver::Answer::Repeat,
        Answer::Cancel => driver::Answer::Cancel,
        Answer::KeepOriginal => driver::Answer::KeepOriginal,
        Answer::KeepCopy => driver::Answer::KeepCopy,
        Answer::KeepBoth => driver::Answer::KeepBoth,
    };
    match (engine, conflict) {
        (driver::Answer::KeepOriginal, Some((original, _))) => {
            Some(format!("keep-node:{}", original.node_id))
        }
        (driver::Answer::KeepCopy, Some((_, copy))) => Some(format!("keep-node:{}", copy.node_id)),
        (driver::Answer::KeepBoth, Some(_)) => Some("keep-both".to_owned()),
        (
            driver::Answer::KeepOriginal | driver::Answer::KeepCopy | driver::Answer::KeepBoth,
            None,
        ) => None,
        (engine, _) => Some(engine.as_str().to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn recovery_payload(cause: Option<serde_json::Value>) -> driver::Question {
        let mut payload = serde_json::json!({
            "id": "recovery", "space_id": "s1", "asked_at_ms": 8,
            "kind": { "type": "recovery", "group": "created_after_restore_point" },
            "items": []
        });
        if let Some(cause) = cause {
            payload["recovery_cause"] = cause;
        }
        let response: koidrive_ipc::Response = serde_json::from_value(serde_json::json!({
            "response": "questions", "questions": [payload]
        }))
        .unwrap();
        let koidrive_ipc::Response::Questions { mut questions } = response else {
            unreachable!();
        };
        questions.remove(0)
    }

    #[test]
    fn recovery_explanations_use_only_the_cause_the_daemon_sent() {
        for (wire, expected) in [
            ("space_rewound", RecoveryTrigger::SpaceRewound),
            ("store_rolled_back", RecoveryTrigger::StoreRolledBack),
            ("store_older_than_disk", RecoveryTrigger::StoreOlderThanDisk),
            ("space_imported", RecoveryTrigger::SpaceImported),
            ("no_sync_state", RecoveryTrigger::NoSyncState),
        ] {
            let asked = recovery_payload(Some(serde_json::json!({
                "trigger": wire, "restore_point_ms": 1_794_442_424_000_i64
            })));
            let shown = question(asked).unwrap();
            assert_eq!(
                shown.kind,
                QuestionKind::Recovery {
                    cause: RecoveryCause {
                        trigger: expected,
                        restore_point_ms: Some(1_794_442_424_000),
                    },
                    group: RecoveryGroup::CreatedAfterRestorePoint,
                }
            );
            assert_eq!(shown.answers, [Answer::Keep, Answer::Discard]);
        }
        for cause in [
            None,
            Some(serde_json::json!({})),
            Some(serde_json::json!({
                "trigger": "a_future_cause", "restore_point_ms": 42
            })),
            Some(serde_json::json!({ "trigger": "unknown", "restore_point_ms": 42 })),
        ] {
            let shown = question(recovery_payload(cause)).unwrap();
            assert_eq!(
                shown.kind,
                QuestionKind::Recovery {
                    cause: RecoveryCause {
                        trigger: RecoveryTrigger::Unknown,
                        restore_point_ms: None,
                    },
                    group: RecoveryGroup::CreatedAfterRestorePoint,
                }
            );
            assert_eq!(shown.answers, [Answer::Keep, Answer::Discard]);
        }
    }

    #[test]
    fn a_conflict_is_a_question_with_both_files_and_a_halted_folder_stays_a_problem() {
        let asked = |items: serde_json::Value| -> driver::Question {
            serde_json::from_value(serde_json::json!({
                "id": "q-conflict", "space_id": "s1", "asked_at_ms": 7,
                "kind": {
                    "type": "conflict",
                    "original": { "node_id": "original", "revisions": { "content_rev": 1, "meta_rev": 1 } },
                    "copy": { "node_id": "copy", "revisions": { "content_rev": 1, "meta_rev": 1 } }
                },
                "items": items
            }))
            .unwrap()
        };
        let copy = serde_json::json!({
            "node_id": "copy", "path": "notes (conflict).txt", "files": 1, "device_id": "d2",
            "size": 2_048, "modified_at_ms": 1_794_442_424_000_i64
        });
        let original = serde_json::json!({
            "node_id": "original", "path": "notes.txt", "files": 1, "device_id": "d1",
            "size": 1_024, "modified_at_ms": 1_794_442_400_000_i64
        });
        let status = status_of(&[DriverFolderState::Halted]);
        let mut seen = Seen::new();
        let halted_only = |state: &SessionState| {
            matches!(
                state.problems.as_slice(),
                [Problem {
                    kind: ProblemKind::FolderHalted { .. },
                    ..
                }]
            )
        };

        let conflict = asked(serde_json::json!([copy, original]));
        let state = session_state(&status, &[conflict], true, &mut seen);
        let [question] = state.questions.as_slice() else {
            panic!("one question: {:?}", state.questions);
        };
        assert_eq!(
            (question.id.as_str(), question.space_id.as_str()),
            ("q-conflict", "s1")
        );
        assert_eq!(question.asked_at_ms, 7);
        assert_eq!(
            question.kind,
            QuestionKind::Conflict {
                original: ConflictSide {
                    node_id: "original".into(),
                    path: "notes.txt".into(),
                    size: 1_024,
                    modified_at_ms: 1_794_442_400_000,
                },
                copy: ConflictSide {
                    node_id: "copy".into(),
                    path: "notes (conflict).txt".into(),
                    size: 2_048,
                    modified_at_ms: 1_794_442_424_000,
                },
            }
        );
        assert_eq!(
            question.answers,
            [Answer::KeepOriginal, Answer::KeepCopy, Answer::KeepBoth]
        );
        assert!(halted_only(&state), "{:?}", state.problems);

        let without = |item: &serde_json::Value, field: &str| {
            let mut item = item.clone();
            item.as_object_mut().unwrap().remove(field);
            item
        };
        for items in [
            serde_json::json!([copy]),
            serde_json::json!([original]),
            serde_json::json!([without(&copy, "size"), original]),
            serde_json::json!([copy, without(&original, "modified_at_ms")]),
        ] {
            let state = session_state(&status, &[asked(items.clone())], true, &mut seen);
            assert!(state.questions.is_empty(), "{items}");
            assert!(halted_only(&state), "{:?}", state.problems);
        }

        let resolved = session_state(&status, &[], true, &mut seen);
        assert!(resolved.questions.is_empty());
        assert!(halted_only(&resolved), "{:?}", resolved.problems);
    }

    fn folder_in(state: DriverFolderState, problem: Option<&str>) -> driver::FolderStatus {
        driver::FolderStatus {
            recovery: None,
            space_id: "0190a5b0-0000-7000-8000-000000000001".into(),
            space_name: "Documents".into(),
            path: "/Users/ada/Documents".into(),
            wait: None,
            state,
            history_id: None,
            last_seq: None,
            head_seq: None,
            remote_nodes: 0,
            local_entries: None,
            questions: 0,
            problem: problem.map(str::to_owned),
            progress: None,
            held_disks: Vec::new(),
        }
    }

    fn status_of(states: &[DriverFolderState]) -> Status {
        Status {
            account: Some(driver::Account {
                server: "https://files.example.com/".into(),
                user_id: "u1".into(),
                user_name: None,
                device_id: "d1".into(),
                device_name: "Mac".into(),
            }),
            running: true,
            paused: false,
            version: None,
            update: driver::Update::Unknown,
            folders: states.iter().map(|state| folder_in(*state, None)).collect(),
            progress: None,
        }
    }

    #[test]
    fn the_session_status_follows_its_folders_and_the_daemon() {
        use DriverFolderState::*;
        let status = |status: &Status, started| {
            session_state(status, &[], started, &mut Default::default()).status
        };
        let up = status_of(&[UpToDate]);
        assert_eq!(status(&up, false), SyncStatus::Stopped);
        let paused = Status {
            paused: true,
            ..up.clone()
        };
        assert_eq!(status(&paused, true), SyncStatus::Paused);
        assert_eq!(status(&status_of(&[]), true), SyncStatus::NeedsSetup);
        let signed_out = Status {
            account: None,
            running: false,
            ..status_of(&[Halted])
        };
        assert_eq!(status(&signed_out, true), SyncStatus::NeedsSetup);
        let no_daemon = Status {
            running: false,
            ..up.clone()
        };
        assert_eq!(status(&no_daemon, true), SyncStatus::Stopped);
        for (states, expected) in [
            (vec![UpToDate], SyncStatus::Idle),
            (vec![UpToDate, UpToDate], SyncStatus::Idle),
            (vec![Halted], SyncStatus::Halted),
            (vec![UpToDate, Halted], SyncStatus::Halted),
            (vec![CatchingUp, Halted], SyncStatus::Halted),
            (vec![Offline, Halted], SyncStatus::Halted),
            (vec![UpToDate, CatchingUp], SyncStatus::Syncing),
            (vec![Bootstrapping, Connecting], SyncStatus::Syncing),
            (vec![UpToDate, Connecting], SyncStatus::Connecting),
            (vec![Offline, UpToDate], SyncStatus::Offline),
            (vec![Offline, CatchingUp], SyncStatus::Offline),
            (vec![Bootstrapping, Offline], SyncStatus::Offline),
            (vec![Connecting, Offline], SyncStatus::Offline),
        ] {
            assert_eq!(status(&status_of(&states), true), expected, "{states:?}");
        }
        assert_eq!(
            folder(&folder_in(Offline, None)).state,
            FolderState::Offline
        );
        assert_eq!(
            folder(&folder_in(CatchingUp, None)).state,
            FolderState::Syncing
        );
        assert_eq!(
            account(up.account.as_ref().unwrap()).server_url,
            "https://files.example.com"
        );
    }

    #[test]
    fn a_folder_that_cannot_reach_the_server_is_a_problem_dated_when_it_first_appeared() {
        let mut seen = Seen::new();
        let mut status = status_of(&[]);
        let waiting = |why: &str, until_ms| {
            Some(driver::Wait {
                kind: driver::WaitKind::Retry,
                until_ms: Some(until_ms),
                unreachable: Some(driver::Unreachable {
                    why: why.into(),
                    no_internet: false,
                }),
            })
        };
        status.folders = vec![driver::FolderStatus {
            wait: waiting("the server's address could not be found", 1_000),
            ..folder_in(DriverFolderState::Offline, None)
        }];
        let first = problems(&status, &mut seen);
        let kinds: Vec<_> = (first.iter())
            .map(|problem| (problem.path.clone(), problem.kind.clone()))
            .collect();
        assert_eq!(
            kinds,
            [(
                None,
                ProblemKind::ServerUnreachable {
                    reason: "the server's address could not be found".into(),
                    retry_at_ms: Some(1_000),
                }
            )]
        );

        std::thread::sleep(Duration::from_millis(5));
        status.folders[0].wait = waiting("the connection timed out", 3_000);
        let later = problems(&status, &mut seen);
        assert_eq!(later[0].since_ms, first[0].since_ms);

        status.folders[0].wait =
            (waiting("the connection timed out", 3_000)).map(|wait| driver::Wait {
                until_ms: None,
                ..wait
            });
        let trying = problems(&status, &mut seen);
        assert_eq!(trying[0].since_ms, first[0].since_ms);
        assert_eq!(
            trying[0].kind,
            ProblemKind::ServerUnreachable {
                reason: "the connection timed out".into(),
                retry_at_ms: None,
            }
        );

        status.folders[0].wait = None;
        assert!(problems(&status, &mut seen).is_empty());
        assert!(seen.is_empty());
    }

    #[test]
    fn each_disk_a_folder_holds_is_a_problem_at_its_mount_point() {
        let mut seen = Seen::new();
        let mut status = status_of(&[]);
        status.folders = vec![driver::FolderStatus {
            held_disks: vec![
                driver::HeldDisk {
                    path: "Disk".into(),
                    kind: driver::HeldDiskKind::NotConnected,
                },
                driver::HeldDisk {
                    path: "Photos/Card".into(),
                    kind: driver::HeldDiskKind::MountedOver,
                },
            ],
            ..folder_in(DriverFolderState::UpToDate, None)
        }];
        let space = Some(status.folders[0].space_id.clone());
        let found: Vec<_> = (problems(&status, &mut seen).into_iter())
            .map(|problem| (problem.space_id, problem.path, problem.kind))
            .collect();
        assert_eq!(
            found,
            [
                (
                    space.clone(),
                    Some("Disk".into()),
                    ProblemKind::DiskNotConnected
                ),
                (
                    space,
                    Some("Photos/Card".into()),
                    ProblemKind::DiskMountedOver
                ),
            ]
        );
        status.folders[0].held_disks.clear();
        assert!(problems(&status, &mut seen).is_empty());
    }

    #[test]
    fn a_halted_folder_is_a_problem_dated_when_its_reason_first_appeared() {
        let mut seen = Seen::new();
        let mut status = status_of(&[]);
        status.folders = vec![folder_in(
            DriverFolderState::Halted,
            Some("the marker is missing"),
        )];
        let first = problems(&status, &mut seen);
        assert_eq!(first.len(), 1);
        assert_eq!(
            first[0].kind,
            ProblemKind::FolderHalted {
                reason: "the marker is missing".into()
            }
        );
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(problems(&status, &mut seen), first);

        status.folders[0].problem = Some("the folder is missing".into());
        let changed = problems(&status, &mut seen);
        assert!(changed[0].since_ms > first[0].since_ms);

        status.folders[0].state = DriverFolderState::UpToDate;
        assert!(problems(&status, &mut seen).is_empty());
        assert!(seen.is_empty());
    }

    #[test]
    fn questions_keep_their_answers_and_items_and_a_remote_mass_delete_is_a_notice() {
        let asked = |id: &str, kind| driver::Question {
            id: id.into(),
            lineage: id.into(),
            recovery_cause: None,
            space_id: "s1".into(),
            asked_at_ms: 7,
            kind,
            items: vec![
                driver::HeldItem {
                    node_id: "n1".into(),
                    path: "Archive".into(),
                    files: 1_499,
                    device_id: None,
                    device_name: None,
                    size: None,
                    modified_at_ms: None,
                },
                driver::HeldItem {
                    node_id: "n2".into(),
                    path: "notes.txt".into(),
                    files: 1,
                    device_id: None,
                    device_name: None,
                    size: None,
                    modified_at_ms: None,
                },
            ],
        };
        let limits = || vec![driver::Limit::Deletions, driver::Limit::Share];
        let questions = [
            asked(
                "q1",
                driver::QuestionKind::MassDelete {
                    deletions: 1_200,
                    affected_files: 1_500,
                    space_files: 2_000,
                    exceeded: limits(),
                },
            ),
            asked(
                "q2",
                driver::QuestionKind::Recovery {
                    group: driver::RecoveryGroup::RequestedBeforeReset,
                    sent_in: None,
                    cause: None,
                },
            ),
            asked(
                "n1",
                driver::QuestionKind::RemoteMassDelete {
                    deletions: 2,
                    affected_files: 30,
                    space_files: 40,
                    exceeded: limits(),
                },
            ),
        ];
        let state = session_state(&status_of(&[]), &questions, true, &mut Seen::new());
        let limits = vec![
            MassDeleteLimit::Deletions { limit: 1_000 },
            MassDeleteLimit::ShareOfSpace { percent: 50 },
        ];
        let [mass_delete, recovery] = state.questions.as_slice() else {
            panic!("two questions expected: {:?}", state.questions);
        };
        assert_eq!(
            mass_delete.kind,
            QuestionKind::MassDelete {
                deletions: 1_200,
                affected_files: 1_500,
                space_files: 2_000,
                exceeded: limits.clone(),
            }
        );
        assert_eq!(mass_delete.answers, [Answer::Delete, Answer::Restore]);
        assert_eq!(mass_delete.items[0].kind, NodeKind::Folder);
        assert_eq!(mass_delete.items[1].kind, NodeKind::File);
        assert_eq!(recovery.answers, [Answer::Repeat, Answer::Cancel]);
        assert_eq!(
            state.notices,
            [Notice {
                id: "n1".into(),
                space_id: "s1".into(),
                at_ms: 7,
                kind: NoticeKind::RemoteMassDelete {
                    deletions: 2,
                    affected_files: 30,
                    space_files: 40,
                    exceeded: limits,
                },
            }]
        );
    }

    #[test]
    fn each_answer_is_the_word_koi_resolve_takes() {
        for (answer, expected) in [
            (Answer::Delete, "delete"),
            (Answer::Restore, "restore"),
            (Answer::Keep, "keep"),
            (Answer::Discard, "discard"),
            (Answer::Publish, "publish"),
            (Answer::Repeat, "repeat"),
            (Answer::Cancel, "cancel"),
        ] {
            assert_eq!(word(answer, None).as_deref(), Some(expected));
        }
        for answer in [Answer::KeepOriginal, Answer::KeepCopy, Answer::KeepBoth] {
            assert_eq!(word(answer, None), None);
        }
        let conflict: driver::QuestionKind = serde_json::from_value(serde_json::json!({
            "type": "conflict",
            "original": { "node_id": "original", "revisions": { "content_rev": 1, "meta_rev": 1 } },
            "copy": { "node_id": "copy", "revisions": { "content_rev": 1, "meta_rev": 1 } }
        }))
        .unwrap();
        for (answer, expected) in [
            (Answer::KeepOriginal, "keep-node:original"),
            (Answer::KeepCopy, "keep-node:copy"),
            (Answer::KeepBoth, "keep-both"),
            (Answer::Delete, "delete"),
        ] {
            assert_eq!(word(answer, Some(&conflict)).as_deref(), Some(expected));
        }
    }
}

#[cfg(test)]
mod unreachable_tests {
    use super::*;

    #[test]
    fn a_device_without_internet_is_its_own_problem() {
        let mut status = Status {
            folders: vec![driver::FolderStatus {
                space_id: "abc".into(),
                space_name: "Documents".into(),
                path: "/Users/ada/Documents".into(),
                state: DriverFolderState::Offline,
                history_id: None,
                last_seq: None,
                head_seq: None,
                remote_nodes: 0,
                local_entries: None,
                questions: 0,
                problem: Some(
                    "no internet connection; trying again at 2026-11-14 12:00 UTC".into(),
                ),
                progress: None,
                held_disks: Vec::new(),
                wait: Some(driver::Wait {
                    kind: driver::WaitKind::Retry,
                    until_ms: Some(7),
                    unreachable: Some(driver::Unreachable {
                        why: "there is no internet connection".into(),
                        no_internet: true,
                    }),
                }),
                recovery: None,
            }],
            running: true,
            ..Status::default()
        };
        let listed = problems(&status, &mut Seen::new());
        let kinds: Vec<_> = listed.iter().map(|problem| problem.kind.clone()).collect();
        assert_eq!(
            kinds,
            [ProblemKind::NoInternet {
                retry_at_ms: Some(7)
            }]
        );
        status.running = false;
        assert_eq!(problems(&status, &mut Seen::new()), []);
    }

    #[test]
    fn a_stopped_or_halted_folder_is_not_a_server_unreachable_problem() {
        for state in [DriverFolderState::Stopped, DriverFolderState::Halted] {
            let status = Status {
                folders: vec![driver::FolderStatus {
                    space_id: "abc".into(),
                    space_name: "Documents".into(),
                    path: "/Users/ada/Documents".into(),
                    state,
                    history_id: None,
                    last_seq: None,
                    head_seq: None,
                    remote_nodes: 0,
                    local_entries: None,
                    questions: 0,
                    problem: Some("the folder is missing".into()),
                    progress: None,
                    held_disks: Vec::new(),
                    wait: Some(driver::Wait {
                        kind: driver::WaitKind::Retry,
                        until_ms: Some(1),
                        unreachable: Some(driver::Unreachable {
                            why: "the server's address could not be found".into(),
                            no_internet: false,
                        }),
                    }),
                    recovery: None,
                }],
                ..Status::default()
            };
            let listed = problems(&status, &mut Seen::new());
            assert!(
                !listed
                    .iter()
                    .any(|problem| matches!(problem.kind, ProblemKind::ServerUnreachable { .. })),
                "{state:?}: {listed:?}"
            );
        }
    }
}
