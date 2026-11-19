// SPDX-License-Identifier: Apache-2.0

use koidrive_engine::classify::Trees;
use koidrive_engine::commit::Commits;
use koidrive_engine::effects::{BatchLimit, Effect, Effects};
use koidrive_engine::first_link::{History, Marks};
use koidrive_engine::ids::IdGen;
use koidrive_engine::question::{
    Answer, QuestionKind, RecoveryCause, RecoveryGroup, RecoveryTrigger,
};
use koidrive_engine::tree::{Content, FileIdentity, Kind, LocalNode, RemoteNode, Shape};
use koidrive_proto::v1::op;

const ROOT: &str = "root";
const NOW: u64 = 1_794_268_740_000;
const WRITTEN_AT: i64 = 1_794_268_747_234_567_891;

fn file(digest: u8, mtime_ns: i64) -> Shape {
    Shape {
        parent: ROOT.into(),
        name: "a".into(),
        kind: Kind::File,
        content: Some(Content {
            size: 3,
            mtime_ns,
            manifest_digest: vec![digest; 32],
            ..Default::default()
        }),
    }
}

fn version(shape: Shape, version_id: &str, made_over: &[&str], rev: u64) -> RemoteNode {
    RemoteNode {
        made_over: made_over.iter().map(|id| (*id).into()).collect(),
        shape,
        trashed: false,
        content_rev: rev,
        edit_rev: rev,
        meta_rev: rev,
        meta_edit_rev: rev,
        trash_group: String::new(),
        version_id: version_id.into(),
        pending_for: None,
        pending_by: None,
    }
}

fn rolled_back_under_a_later_edit(on_disk_at: i64) -> (Trees, Commits, IdGen) {
    let mut trees = Trees::new(ROOT);
    let local = LocalNode {
        left_out_when_placed: false,
        content_from_server: false,
        shape: file(4, on_disk_at),
        identity: FileIdentity {
            ino: 1,
            birth_ns: Some(0),
            handle: None,
        },
        parked: false,
    };
    trees.set_local("n".into(), local);
    trees.set_synced("n".into(), Some(file(4, WRITTEN_AT)));
    trees.set_remote("n".into(), version(file(4, WRITTEN_AT), "v4", &[], 1));
    trees.rules_scanned();
    trees.settle(&Default::default());
    trees.set_remote("n".into(), version(file(5, 0), "v5", &["v4"], 2));
    trees.settle(&Default::default());
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    let cause = RecoveryCause::of(RecoveryTrigger::StoreOlderThanDisk);
    commits.enter_recovery(cause);
    assert_eq!(
        commits.build(&mut trees, &mut ids, NOW, "old", "me", 9),
        None
    );
    commits.recover(
        &mut trees,
        0,
        false,
        &cause,
        &History::new(),
        &Marks::new(),
        &mut ids,
        NOW,
    );
    trees.settle(commits.aliases());
    (trees, commits, ids)
}

fn effects(trees: &mut Trees, ids: &mut IdGen) -> Vec<(String, Effect)> {
    let mut effects = Effects::default();
    let mut done = Vec::new();
    for ino in 100.. {
        trees.settle(&Default::default());
        let Some(record) = (effects.plan(trees, None, ids, NOW, BatchLimit::ONE).first()).cloned()
        else {
            break;
        };
        let identity = FileIdentity {
            ino,
            birth_ns: Some(0),
            handle: None,
        };
        effects
            .done(trees, None, &record.effect_id, Some(identity))
            .unwrap();
        done.push((record.node, record.effect));
    }
    done
}

#[test]
fn a_file_written_again_with_the_bytes_s_holds_before_a_store_rollback_is_sent_when_published() {
    let (mut trees, mut commits, mut ids) =
        rolled_back_under_a_later_edit(WRITTEN_AT + 2_000_000_123);
    let [question] = commits.questions() else {
        panic!("{:?}", commits.questions());
    };
    let question = question.clone();
    assert!(matches!(
        question.kind,
        QuestionKind::Recovery {
            group: RecoveryGroup::ChangedOnThisDevice,
            ..
        }
    ));
    assert!(trees.held("n"));
    assert!(effects(&mut trees, &mut ids).is_empty());
    assert!(commits.decide(&question.id, Answer::Publish));
    let commit = commits
        .build(&mut trees, &mut ids, NOW, "new", "me", 1)
        .unwrap();
    let sent: Vec<_> = commit.ops.iter().flat_map(|op| op.op.clone()).collect();
    let [op::Op::UpdateContent(update)] = &sent[..] else {
        panic!("{sent:?}");
    };
    assert_eq!(update.node_id, "n");
    assert_eq!(update.content.as_ref().unwrap().manifest_digest, [4; 32]);
}

#[test]
fn a_file_whose_time_only_lost_precision_after_a_store_rollback_follows_the_later_edit() {
    let (mut trees, mut commits, mut ids) =
        rolled_back_under_a_later_edit(WRITTEN_AT - WRITTEN_AT % 1_000_000_000);
    assert!(commits.questions().is_empty(), "{:?}", commits.questions());
    let done = effects(&mut trees, &mut ids);
    let [(node, Effect::WriteFile { content, .. })] = &done[..] else {
        panic!("{done:?}");
    };
    assert_eq!(
        (node.as_str(), &content.manifest_digest),
        ("n", &vec![5; 32])
    );
    assert!(
        commits
            .build(&mut trees, &mut ids, NOW, "new", "me", 1)
            .is_none()
    );
}
