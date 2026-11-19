// SPDX-License-Identifier: Apache-2.0

use crate::error::CliError;
use crate::formatting::{count, local_time, need_you, now_ms, printable};
use crate::profile::unexpected;
use crate::report::{AnsweredData, Decision, Done};
use crate::{ipc, output};
use koidrive_driver::{
    Answer, Credential, Driver, DriverError, FolderStatus, ItemKind, Limit, MAX_DELETIONS, Paths,
    Question, QuestionKind, RecoveryGroup, RestoreRequest,
};
use koidrive_ipc::{Request, Response};
use serde_json::Value;

pub(crate) async fn questions(paths: &Paths) -> Result<Vec<Question>, CliError> {
    match ipc::run(paths, Request::Questions).await? {
        Response::Questions { questions } => Ok(questions),
        other => Err(unexpected(other)),
    }
}

/// The decision `id` once it has an answer, waiting for sync or taken by it, for an answer given again.
async fn answered_question(paths: &Paths, id: &str) -> Result<Option<Question>, CliError> {
    let request = Request::AnsweredQuestion {
        question_id: id.to_owned(),
    };
    match ipc::run(paths, request).await? {
        Response::AnsweredQuestion { question } => Ok(question),
        other => Err(unexpected(other)),
    }
}

/// `koi resolve`: every pending decision, or the answer to one. An answer is recorded; sync carries it out later.
pub(crate) async fn resolve(
    paths: &Paths,
    id: Option<String>,
    word: Option<String>,
    json: bool,
) -> Result<Done, CliError> {
    let questions = questions(paths).await?;
    let (Some(id), Some(word)) = (id, word) else {
        let summary = if questions.is_empty() || json || !output::concise("resolve") {
            questions_text(&questions)
        } else {
            let credential = Credential::load(&paths.credential()).ok().flatten();
            let (folders, account) = match ipc::ask(paths, &Request::Status).await {
                Ok(Some(Response::Status { status })) => (status.folders, status.account),
                _ => (Driver::linked_folders(paths).unwrap_or_default(), None),
            };
            let device = account
                .as_ref()
                .map(|account| account.device_id.as_str())
                .or_else(|| {
                    credential
                        .as_ref()
                        .map(|credential| credential.device_id.as_str())
                });
            questions
                .iter()
                .map(|question| {
                    question_text_presented(
                        question,
                        QuestionPresentation::Standalone {
                            folder: folders
                                .iter()
                                .find(|folder| folder.space_id == question.space_id),
                            device,
                        },
                    )
                })
                .collect()
        };
        let mut done = Done::ok(Value::Null, questions_text(&questions)).human(summary, "");
        done.decisions = questions.iter().map(Decision::of).collect();
        return Ok(done);
    };
    let question = match questions.into_iter().find(|question| question.id == id) {
        Some(question) => question,
        None => answered_question(paths, &id)
            .await?
            .ok_or_else(|| DriverError::QuestionNotFound(id.clone()))?,
    };
    let answer = answer_to(&question, &word)
        .map_err(|message| CliError::usage(message).then(&["koi", "resolve", "--json"]))?;
    let request = Request::Answer {
        question_id: id.clone(),
        answer,
    };
    match ipc::run(paths, request).await? {
        Response::Answered => Ok(Done::accepted(
            AnsweredData {
                decision_id: id,
                answer: word,
            },
            "Resolved.\n",
        )
        .human("Choice saved. Sync will apply it.\n", "")),
        Response::AlreadyAnswered => Ok(Done::ok(
            AnsweredData {
                decision_id: id,
                answer: word.clone(),
            },
            format!("Already resolved with {}.\n", printable(&word)),
        )
        .human(
            format!("This choice was already saved as {}.\n", printable(&word)),
            "",
        )
        .unchanged()),
        other => Err(unexpected(other)),
    }
}

fn questions_text(questions: &[Question]) -> String {
    if questions.is_empty() {
        return "No decisions are waiting.\n".to_owned();
    }
    questions.iter().map(question_text).collect()
}

pub(crate) const RESOLVE: &str = "koi resolve";

pub(crate) const ITEMS_SHOWN: usize = 20;

fn question_summary(question: &Question) -> String {
    let why = |exceeded: &[Limit]| {
        let limits: Vec<String> = (exceeded.iter())
            .map(|limit| match limit {
                Limit::Deletions => format!("more than {MAX_DELETIONS} deletions in 10 minutes"),
                Limit::Share => "more than half of the space in 10 minutes".to_owned(),
            })
            .collect();
        match &limits[..] {
            [] => String::new(),
            limits => format!(", {}", limits.join(" and ")),
        }
    };
    match &question.kind {
        QuestionKind::MassDelete {
            deletions,
            affected_files,
            space_files,
            exceeded,
        } => format!(
            "held {deletions} deletions that would remove {affected_files} of {space_files} files{}",
            why(exceeded)
        ),
        QuestionKind::RemoteMassDelete {
            deletions,
            affected_files,
            space_files,
            exceeded,
        } => format!(
            "applied {deletions} deletions from other devices that removed {affected_files} of \
             {space_files} files{}",
            why(exceeded)
        ),
        QuestionKind::AmbiguousDisappearance => {
            "Koidrive couldn't tell whether these were moved or deleted. Other devices keep them \
             until you decide"
                .to_owned()
        }
        QuestionKind::Recovery { group, .. } => match group {
            RecoveryGroup::CreatedAfterRestorePoint => {
                "files created after the restore point, which the restore removed from the server"
            }
            RecoveryGroup::ChangedOnThisDevice => {
                "files changed on this device since it last synced"
            }
            RecoveryGroup::CreatedOnThisDevice => {
                "files on this device that the space does not hold"
            }
            RecoveryGroup::DeletedOnThisDevice => {
                "files deleted on this device that the server still holds"
            }
            RecoveryGroup::RequestedBeforeReset => {
                "restores asked for before recovery began, which the server does not show"
            }
        }
        .to_owned(),
        QuestionKind::Conflict { original, .. } => format!(
            "{} has a conflict copy",
            conflict_path(question, &original.node_id.to_string())
        ),
    }
}

fn first_path(question: &Question) -> String {
    if let QuestionKind::Conflict { .. } = question.kind {
        return String::new();
    }
    (question.items.first())
        .map(|item| format!("; first path: {}", printable(item.path.unquoted())))
        .unwrap_or_default()
}

fn conflict_path(question: &Question, node_id: &str) -> String {
    (question.items.iter())
        .find(|item| item.node_id == node_id)
        .map(|item| output::command_word(item.path.unquoted()))
        .unwrap_or_else(|| format!("node {}", printable(node_id)))
}

fn conflict_files(question: &Question, indent: &str, this_device: Option<&str>) -> String {
    let QuestionKind::Conflict { original, copy, .. } = &question.kind else {
        return String::new();
    };
    [("Original", original), ("Conflict copy", copy)]
        .into_iter()
        .map(|(role, file)| {
            let item = (question.items.iter()).find(|item| item.node_id == file.node_id);
            let device_id = item.and_then(|item| item.device_id.as_deref());
            let name = (item.and_then(|item| item.device_name.as_deref())).map(printable);
            let writer = match (device_id, name) {
                (None, _) => "an unknown device".to_owned(),
                (Some(id), Some(name)) if Some(id) == this_device => {
                    format!("{name} (this device)")
                }
                (Some(_), Some(name)) => name,
                (Some(id), None) if Some(id) == this_device => "this device".to_owned(),
                (Some(_), None) if this_device.is_some() => "another device".to_owned(),
                (Some(_), None) => "a device".to_owned(),
            };
            format!(
                "{indent}{role}: {}, written by {writer}\n",
                conflict_path(question, &file.node_id.to_string())
            )
        })
        .collect()
}

const KEEP_BOTH: &str = "keep-both";

pub(crate) struct Choice {
    pub(crate) word: String,
    pub(crate) effect: String,
    answer: Answer,
}

/// The answers a decision takes, in the order to offer them. A conflict's answers name the node ID of the file to keep,
/// so that no one has to tell which device's file is "mine".
pub(crate) fn choices(question: &Question) -> Vec<Choice> {
    let QuestionKind::Conflict { original, copy, .. } = &question.kind else {
        return (question.kind.answers().iter())
            .map(|&answer| Choice {
                word: answer.as_str().to_owned(),
                effect: answer_effect(answer).to_owned(),
                answer,
            })
            .collect();
    };
    let kept = conflict_path(question, &original.node_id.to_string());
    let copied = conflict_path(question, &copy.node_id.to_string());
    vec![
        Choice {
            word: format!("keep-node:{}", original.node_id),
            effect: format!("Keep {kept} and move {copied} to the trash"),
            answer: Answer::KeepOriginal,
        },
        Choice {
            word: format!("keep-node:{}", copy.node_id),
            effect: format!(
                "Give {kept} the content of {copied}, keeping the old content in its version history, and move \
                 {copied} to the trash"
            ),
            answer: Answer::KeepCopy,
        },
        Choice {
            word: KEEP_BOTH.to_owned(),
            effect: "Keep both files".to_owned(),
            answer: Answer::KeepBoth,
        },
    ]
}

pub(crate) fn answer_to(question: &Question, word: &str) -> Result<Answer, String> {
    let choices = choices(question);
    if let Some(choice) = choices.iter().find(|choice| choice.word == word) {
        return Ok(choice.answer);
    }
    let words: Vec<String> = (choices.iter())
        .map(|choice| printable(&choice.word))
        .collect();
    Err(format!(
        "{} is not an answer to {}; use {}",
        printable(word),
        printable(&question.id),
        words.join(" or ")
    ))
}

fn resolve_commands(question: &Question, indent: &str) -> String {
    let id = printable(&question.id);
    (choices(question).iter())
        .map(|choice| {
            format!(
                "{indent}{}:\n{indent}  {RESOLVE} {id} {}\n",
                choice.effect,
                printable(&choice.word)
            )
        })
        .collect()
}

fn answer_effect(answer: Answer) -> &'static str {
    match answer {
        Answer::Delete => "Delete these files on the server and other devices",
        Answer::Restore => "Download these files to this device again",
        Answer::Keep => "Put these files back on the server",
        Answer::Discard => "Move this device's copies to the local trash and follow the server",
        Answer::Publish => "Send this device's version to the server",
        Answer::Repeat => "Send the request again",
        Answer::Cancel => "Drop the request",
        Answer::Dismiss => "Close this notice",
        Answer::KeepOriginal | Answer::KeepCopy | Answer::KeepBoth => {
            unreachable!("a conflict's answers name its files")
        }
    }
}

pub(crate) fn question_brief(question: &Question, this_device: Option<&str>) -> String {
    format!(
        "  {}: {}{}\n{}{}",
        printable(&question.id),
        question_summary(question),
        first_path(question),
        conflict_files(question, "    ", this_device),
        resolve_commands(question, "  ")
    )
}

pub(crate) fn question_text(question: &Question) -> String {
    question_text_presented(question, QuestionPresentation::Detailed)
}

enum QuestionPresentation<'a> {
    Detailed,
    Standalone {
        folder: Option<&'a FolderStatus>,
        device: Option<&'a str>,
    },
}

fn question_text_presented(question: &Question, presentation: QuestionPresentation<'_>) -> String {
    let (scope, this_device, inspection) = match presentation {
        QuestionPresentation::Detailed => (
            format!("  space {}", printable(&question.space_id)),
            None,
            "",
        ),
        QuestionPresentation::Standalone { folder, device } => (
            match folder {
                Some(folder) => format!("  folder {}", output::folder(folder)),
                None => format!("  space {}", printable(&question.space_id)),
            },
            device,
            if folder.is_none() {
                "  Inspect context: koi status --verbose\n"
            } else {
                ""
            },
        ),
    };
    let mut text = format!(
        "{}{scope}  {}\n{inspection}{}{}",
        printable(&question.id),
        question_summary(question),
        conflict_files(question, "    ", this_device),
        resolve_commands(question, "  ")
    );
    if let QuestionKind::Conflict { .. } = question.kind {
        return text;
    }
    for item in question.items.iter().take(ITEMS_SHOWN) {
        let folder = item.kind == Some(ItemKind::Folder);
        let what = match item.restore {
            Some(RestoreRequest::Version {
                saved_at_ms: Some(saved_at_ms),
                ..
            }) => format!(
                "an earlier version from {}",
                local_time(saved_at_ms, now_ms())
            ),
            Some(RestoreRequest::Version { .. }) => "an earlier version".to_owned(),
            Some(RestoreRequest::FromTrash { .. }) if folder => {
                format!("{} from the trash", count(item.files, "file"))
            }
            Some(RestoreRequest::FromTrash { .. }) => "from the trash".to_owned(),
            None => count(item.files, "file"),
        };
        let slash = if folder { "/" } else { "" };
        text += &format!("  {}{slash} ({what})\n", printable(item.path.unquoted()));
    }
    let more = question.items.len().saturating_sub(ITEMS_SHOWN);
    if more > 0 {
        text += &format!("  and {more} more; {RESOLVE} --json lists them all\n");
    }
    text
}

pub(crate) fn question_notice(question: &Question) -> String {
    format!(
        "{}{}. Run {RESOLVE}.",
        question_summary(question),
        first_path(question)
    )
}

/// The line every command but `status`, `resolve` and `daemon` prints on stderr while decisions are pending.
pub(crate) fn banner(questions: &[Question]) -> Option<String> {
    let n = questions.len() as u64;
    (n > 0).then(|| format!("{}; run {RESOLVE}", need_you(n)))
}

/// The decisions pending in the profile, asked of the daemon. Without a daemon there are none to report, because a
/// command run on its own leaves the profile as it found it.
pub(crate) async fn pending_questions(paths: &Paths) -> Vec<Question> {
    let asked = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        koidrive_ipc::ask(paths, &Request::Questions),
    );
    match asked.await {
        Ok(Ok(Some(Response::Questions { questions }))) => questions,
        _ => Vec::new(),
    }
}
