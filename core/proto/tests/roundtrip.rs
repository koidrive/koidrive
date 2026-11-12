// SPDX-License-Identifier: Apache-2.0

use koidrive_proto::v1::{Commit, Move, Op, Place, op};
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
        }],
        receipts: vec![],
        ..Default::default()
    };
    let bytes = commit.encode_to_vec();
    assert_eq!(Commit::decode(bytes.as_slice()).unwrap(), commit);
}
