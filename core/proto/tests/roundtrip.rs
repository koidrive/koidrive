// SPDX-License-Identifier: Apache-2.0

use koidrive_proto::v1::{Commit, MarkGitTracked, Move, NodeState, Op, Place, op};
use prost::Message;

#[test]
fn commit_round_trips_through_binary_encoding() {
    let commit = Commit {
        commit_id: "0190f3a0-0000-7000-8000-000000000001".into(),
        base_seq: 7,
        ops: vec![Op {
            op: Some(op::Op::Move(Move {
                node_id: "0190f3a0-0000-7000-8000-000000000002".into(),
                new_parent_id: "0190f3a0-0000-7000-8000-000000000003".into(),
                new_name: "notes.txt".into(),
                from: Some(Place {
                    parent_id: "0190f3a0-0000-7000-8000-000000000004".into(),
                    name: "notes 1.txt".into(),
                }),
                pending_for: String::new(),
            })),
            ..Default::default()
        }],
        receipts: vec![],
        ..Default::default()
    };
    let bytes = commit.encode_to_vec();
    assert_eq!(Commit::decode(bytes.as_slice()).unwrap(), commit);
}

#[test]
fn git_tracked_marks_use_the_field_numbers_of_rfc_0033() {
    let node = NodeState {
        git_tracked: true,
        git_tracked_rev: 7,
        ..Default::default()
    };
    assert_eq!(node.encode_to_vec(), [0x80, 0x01, 0x01, 0x88, 0x01, 0x07]);
    let op = Op {
        op: Some(op::Op::MarkGitTracked(MarkGitTracked {
            node_id: "n".into(),
            tracked: true,
        })),
        ..Default::default()
    };
    assert_eq!(
        op.encode_to_vec(),
        [0x4a, 0x05, 0x0a, 0x01, 0x6e, 0x10, 0x01]
    );
    assert_eq!(Op::decode(op.encode_to_vec().as_slice()).unwrap(), op);
}
