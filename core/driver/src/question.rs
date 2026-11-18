// SPDX-License-Identifier: Apache-2.0

//! Questions the engine asks the person (`AskUser`, design §1) and their answers. The engine persists every
//! question with what it holds, in the folder's store, and asks again after a restart until it is answered (§2.8).
//! These types mirror the macOS surface in `core/ffi` (`Question`, `QuestionKind`, `Answer`), so one driver serves the
//! app and the CLI.

pub use koidrive_engine::question::{Answer, Limit, QuestionKind, RecoveryGroup};
use koidrive_proto::v1::VersionInfo;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    /// Unique per question. A question whose held set changes is replaced by one with a new id, so an answer
    /// always refers to the set the person saw.
    pub id: String,
    pub space_id: String,
    pub asked_at_ms: i64,
    pub kind: QuestionKind,
    /// Every node the answer applies to, in path order.
    pub items: Vec<HeldItem>,
    /// The id of the first question in this one's line. A question asked in place of an open, unanswered question of
    /// the same kind, as when its held set changes, continues that question's line; any other question starts its own,
    /// with its id. A client tells a new question from a replaced one by it, even when it never saw the one replaced.
    /// Rows stored before this field read as their own line.
    #[serde(default)]
    pub lineage: String,
    /// The stored cause of a recovery, absent when an older producer did not record it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_cause: Option<RecoveryCause>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryCause {
    #[serde(default)]
    pub trigger: RecoveryTrigger,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore_point_ms: Option<i64>,
}

impl From<&koidrive_engine::question::RecoveryCause> for RecoveryCause {
    fn from(cause: &koidrive_engine::question::RecoveryCause) -> Self {
        use koidrive_engine::question::RecoveryTrigger as Engine;
        RecoveryCause {
            trigger: match cause.trigger {
                Engine::SpaceRewound => RecoveryTrigger::SpaceRewound,
                Engine::StoreRolledBack => RecoveryTrigger::StoreRolledBack,
                Engine::StoreOlderThanDisk => RecoveryTrigger::StoreOlderThanDisk,
            },
            restore_point_ms: cause.restore_point_ms,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryTrigger {
    SpaceRewound,
    StoreRolledBack,
    StoreOlderThanDisk,
    SpaceImported,
    NoSyncState,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldItem {
    pub node_id: String,
    /// The path relative to the linked folder.
    pub path: koidrive_fs::ItemPath,
    /// Files the answer affects through this item: 1 for a file, the files it holds for a folder.
    pub files: u64,
    /// For a file of a conflict, the device that wrote its current version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    /// That device's name, when the account's device list names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
    /// For a file of a conflict, the size of its current version in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// For a file of a conflict, the modification time of its current version, in milliseconds since the epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_at_ms: Option<i64>,
    /// For a restore asked for before recovery, what it brings back. The item's `node_id` is then the restore's ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore: Option<crate::RestoreRequest>,
}

impl HeldItem {
    pub(crate) fn show_current(&mut self, current: Option<VersionInfo>) {
        let content = current
            .as_ref()
            .and_then(|version| version.content.as_ref());
        self.size = content.map(|content| content.size);
        self.modified_at_ms = content.map(|content| content.mtime_ns / 1_000_000);
        self.device_id = current.map(|version| version.device_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_round_trip_through_their_names() {
        for answer in Answer::ALL {
            assert_eq!(answer.as_str().parse::<Answer>(), Ok(answer));
            assert_eq!(
                serde_json::to_string(&answer).unwrap(),
                format!("\"{answer}\"")
            );
        }
        assert!("Delete".parse::<Answer>().is_err());
    }

    #[test]
    fn a_question_serializes_with_a_tagged_kind() {
        let question = Question {
            id: "q1".into(),
            space_id: "s1".into(),
            asked_at_ms: 5,
            kind: QuestionKind::MassDelete {
                deletions: 1_200,
                affected_files: 1_200,
                space_files: 2_000,
                exceeded: vec![Limit::Deletions, Limit::Share],
            },
            items: vec![HeldItem {
                node_id: "n1".into(),
                path: "a/b".into(),
                files: 1,
                device_id: None,
                device_name: None,
                size: None,
                modified_at_ms: None,
                restore: None,
            }],
            lineage: "q1".into(),
            recovery_cause: None,
        };
        let json = serde_json::to_value(&question).unwrap();
        assert_eq!(json["kind"]["type"], "mass_delete");
        assert_eq!(serde_json::from_value::<Question>(json).unwrap(), question);
        assert_eq!(question.kind.answers(), [Answer::Delete, Answer::Restore]);
    }
}
