// SPDX-License-Identifier: Apache-2.0

//! Exporting and restoring the state the store keeps (docs/design/sync-engine.md section 2.8). A
//! valid state restores to the same value, and a state that breaks one invariant is refused with
//! the error that names it.

use koidrive_engine::classify::{Class, Origin, Trees, Waiting};
use koidrive_engine::commit::{Commits, Halt, InFlight, MAX_OPS};
use koidrive_engine::effects::{Effect, EffectRecord, Effects, Location};
use koidrive_engine::guard::{Counted, GuardState, RemoteDeletion, Trash};
use koidrive_engine::ids::uuid_v7;
use koidrive_engine::question::{
    Answer, Held, Limit, Question, QuestionKind, RecoveryCause, Revisions,
};
use koidrive_engine::restore::{Outcome, Rejection, Resolved, RestoreIntent};
use koidrive_engine::state::{
    Agreed, CommitsState, EffectsState, FinishOrigin, MovedHere, PendingFinish, StateError,
    TreesState,
};
use koidrive_engine::tree::{Content, FileIdentity, Kind, LocalNode, RemoteNode, Shape, TreeError};
use koidrive_proto::v1::{Commit, CommitResult, Delete, Move, Op, OpResult, op};

const ROOT: &str = "root";

fn dir(parent: &str, name: &str) -> Shape {
    Shape {
        parent: parent.into(),
        name: name.into(),
        kind: Kind::Dir,
        content: None,
    }
}

fn remote(shape: Shape) -> RemoteNode {
    RemoteNode {
        shape,
        trashed: false,
        content_rev: 1,
        meta_rev: 1,
        meta_edit_rev: 1,
        trash_group: String::new(),
        version_id: String::new(),
        edit_rev: 1,
        pending_for: None,
        pending_by: None,
    }
}

/// Trees as the store loads them, with `a` placed and `b` parked.
fn trees() -> Trees {
    let mut trees = Trees::new(ROOT);
    for (ino, (id, parked)) in [("a", false), ("b", true)].into_iter().enumerate() {
        let shape = dir(ROOT, id);
        let identity = FileIdentity {
            dev: 1,
            ino: ino as u128,
            birth_ns: Some(0),
        };
        trees.set_remote(id.into(), remote(shape.clone()));
        trees.set_local(
            id.into(),
            LocalNode {
                shape: shape.clone(),
                identity,
                parked,
            },
        );
        trees.set_synced(id.into(), Some(shape));
    }
    let deleted = RemoteNode {
        trashed: true,
        meta_rev: 5,
        meta_edit_rev: 5,
        trash_group: "5:0".into(),
        ..remote(dir(ROOT, "t"))
    };
    trees.set_remote("t".into(), deleted);
    trees
}

fn trees_state() -> TreesState {
    TreesState {
        dirty: ["b".into()].into(),
        unsearched: ["b".into()].into(),
        reverted: ["a".into()].into(),
        parked_as: [("b".into(), "c".into())].into(),
        waiting: [(
            "a".into(),
            Waiting {
                shape: Some(dir(ROOT, "x")),
                origin: Origin::Commit(3),
            },
        )]
        .into(),
        watched: [("u".into(), true), ("v".into(), false)].into(),
        watched_positions: [
            (
                "m".into(),
                Some(Agreed {
                    rev: 3,
                    parent: ROOT.into(),
                    name: "m".into(),
                }),
            ),
            ("p".into(), None),
        ]
        .into(),
        trashed_here: [("t".into(), 5)].into(),
        removed_here: [("t".into(), 5)].into(),
        discarded: [("a".into(), dir(ROOT, "x"))].into(),
        outside: ["a".into()].into(),
        lost: ["a".into()].into(),
        displaced: Default::default(),
        edited_here: [("a".into(), 1)].into(),
        moved_here: [(
            "a".into(),
            MovedHere {
                seq: 1,
                upgraded: false,
            },
        )]
        .into(),
        past_positions: [(
            "a".into(),
            vec![Agreed {
                rev: 1,
                parent: ROOT.into(),
                name: "q".into(),
            }],
        )]
        .into(),
        unsent_edits: Default::default(),
        dropped_edits: ["a".into()].into(),
        resends: Default::default(),
        local_changes: ["b".into()].into(),
        replaced_versions: [("a".into(), vec!["v1".into()])].into(),
        kept_originals: [("t".into(), "a".into())].into(),
        unsent_marks: [("a".into(), "A".into())].into(),
        pending_finishes: [(
            "a".into(),
            PendingFinish {
                parent: "root".into(),
                pending: "a (pending)".into(),
                name: "a".into(),
                origin: FinishOrigin::Legacy,
                disk_first: true,
                base: Some(4),
                sending: true,
            },
        )]
        .into(),
    }
}

fn content(digest: u8) -> Content {
    Content {
        size: 3,
        manifest_digest: vec![digest; 32],
        ..Default::default()
    }
}

fn unsent() -> (Trees, TreesState) {
    let mut trees = Trees::new(ROOT);
    let shape = Shape {
        parent: ROOT.into(),
        name: "f".into(),
        kind: Kind::File,
        content: Some(content(2)),
    };
    trees.set_remote("f".into(), remote(shape.clone()));
    let identity = FileIdentity {
        dev: 1,
        ino: 1,
        birth_ns: Some(0),
    };
    let local = LocalNode {
        shape: shape.clone(),
        identity,
        parked: false,
    };
    trees.set_local("f".into(), local);
    trees.set_synced("f".into(), Some(shape));
    let state = TreesState {
        dirty: ["f".into()].into(),
        unsent_edits: ["f".into()].into(),
        ..Default::default()
    };
    (trees, state)
}

#[test]
fn an_unsent_edit_restores_while_l_holds_the_file_and_it_is_dirty() {
    let (mut trees, state) = unsent();
    trees.restore(state.clone()).unwrap();
    assert_eq!(trees.state(), state);
    assert_eq!(trees.class("f"), Class::Local);
    let (mut trees, state) = unsent();
    let clean = TreesState {
        dirty: Default::default(),
        ..state
    };
    assert_eq!(trees.restore(clean), Err(StateError::NotDirty("f".into())));
    let (mut trees, state) = unsent();
    trees.remove_local("f");
    assert_eq!(
        trees.restore(state),
        Err(StateError::NotUnsentEdit("f".into()))
    );
    let (mut trees, mut state) = unsent();
    state.unsent_edits.insert("g".into());
    assert_eq!(
        trees.restore(state),
        Err(StateError::NotUnsentEdit("g".into()))
    );
}

#[test]
fn a_resend_restores_only_while_l_holds_the_node() {
    let (mut trees, mut state) = unsent();
    state.resends.insert("g".into());
    assert_eq!(trees.restore(state), Err(StateError::NotResend("g".into())));
}

#[test]
fn a_local_change_restores_only_while_l_holds_the_node() {
    let (mut trees, mut state) = unsent();
    state.local_changes.insert("g".into());
    assert_eq!(
        trees.restore(state),
        Err(StateError::NotLocalChange("g".into()))
    );
}

/// A restore of the trashed node `n`.
fn restore(n: u128) -> RestoreIntent {
    RestoreIntent::from_trash(uuid_v7(1, n), None, None).unwrap()
}

/// A state with one restore in the commit in flight, one that waits and one that ended.
fn commits_state() -> CommitsState {
    let delete = Delete {
        node_id: "d".into(),
        requires: None,
    };
    let moved = Move {
        node_id: "m".into(),
        new_parent_id: ROOT.into(),
        new_name: "n".into(),
        from: None,
        pending_for: String::new(),
    };
    let ops = vec![
        Op {
            op: Some(op::Op::Delete(delete)),
        },
        Op {
            op: Some(op::Op::Move(moved)),
        },
        Op {
            op: Some(op::Op::Restore(restore(1).op().clone())),
        },
    ];
    CommitsState {
        accepted: 5,
        in_flight: Some(InFlight {
            commit: Commit {
                commit_id: "commit".into(),
                device_seq: 6,
                ops,
                ..Default::default()
            },
            deletes: [("d".into(), vec!["d".into(), "e".into()])].into(),
            restores: [(2, vec!["r".into(), "s".into()])].into(),
            result: Some(CommitResult {
                results: vec![OpResult::default(); 3],
                ..Default::default()
            }),
        }),
        problems: ["p".into()].into(),
        halted: Some(Halt::Recovery(RecoveryCause::rewind(4, 1_700_000_000_000))),
        cap: Some(4),
        split: ["s".into()].into(),
        aliases: [("f".into(), "g".into())].into(),
        restores: [("r1".into(), restore(1)), ("r2".into(), restore(2))].into(),
        resolved: vec![Resolved {
            id: "r0".into(),
            intent: restore(3),
            outcome: Outcome::Rejected(Rejection::NotFound),
        }],
        slots: [(2, 5), (3, 8)].into(),
        questions: questions(),
        published: ["n".into()].into(),
        guard: guard_state(),
    }
}

/// An open question holding a node R holds and one it does not.
fn questions() -> Vec<Question> {
    let revisions = Revisions {
        content_rev: 2,
        meta_rev: 3,
    };
    vec![Question {
        id: "q".into(),
        asked_at_ms: 7,
        kind: QuestionKind::MassDelete {
            deletions: 1,
            affected_files: 1,
            space_files: 1,
            exceeded: vec![Limit::Share],
        },
        held: [(
            "h".into(),
            Held {
                members: [("h".into(), Some(revisions)), ("i".into(), None)].into(),
                files: 1,
            },
        )]
        .into(),
        answer: Some(Answer::Delete),
        shown: Default::default(),
    }]
}

/// A guard with a deletion in the window and a confirmed deletion.
fn guard_state() -> GuardState {
    let held = Held {
        members: [("c".into(), None)].into(),
        files: 1,
    };
    GuardState {
        ledger: [("h".into(), Counted { at_ms: 7, files: 1 })].into(),
        confirmed: [("c".into(), held)].into(),
        confirmed_limits: [Limit::Share].into(),
        remote: [(
            RemoteDeletion {
                top: "r".into(),
                trash: Trash::Group {
                    group: "4:0".into(),
                    seq: 4,
                },
            },
            Counted { at_ms: 8, files: 2 },
        )]
        .into(),
        dismissed: [RemoteDeletion {
            top: "s".into(),
            trash: Trash::Unknown { no_later_than: 6 },
        }]
        .into(),
    }
}

fn effects_state() -> EffectsState {
    let create = Effect::CreateDir {
        at: Location::Parked("a".into()),
    };
    EffectsState {
        in_flight: vec![EffectRecord {
            effect_id: "effect".into(),
            node: "a".into(),
            counter: 4,
            effect: create.clone(),
            after: Some(dir(ROOT, "a")),
        }],
        cleared: 3,
        failed: [("b".into(), create)].into(),
    }
}

/// `value` changed by `change`.
fn broken<T>(mut value: T, change: impl FnOnce(&mut T)) -> T {
    change(&mut value);
    value
}

#[test]
fn trees_restore_a_valid_state_and_refuse_each_broken_one() {
    let mut restored = trees();
    assert_eq!(restored.restore(trees_state()), Ok(()));
    assert_eq!(restored.state(), trees_state());

    let orphan = broken(trees(), |trees| {
        trees.set_remote("o".into(), remote(dir("gone", "o")));
    });
    let downloaded = broken(trees(), |trees| {
        trees.set_remote("n".into(), remote(dir(ROOT, "n")));
    });
    let uploaded = broken(trees(), |trees| {
        let node = LocalNode {
            shape: dir(ROOT, "n"),
            identity: FileIdentity {
                dev: 1,
                ino: 9,
                birth_ns: Some(0),
            },
            parked: false,
        };
        trees.set_local("n".into(), node);
    });
    let held = FileIdentity {
        dev: 1,
        ino: 7,
        birth_ns: None,
    };
    let cases = [
        (
            "a node of R whose parent is missing",
            orphan,
            trees_state(),
            StateError::Tree('R', TreeError::MissingParent("o".into())),
        ),
        (
            "a node only R holds that is not dirty",
            downloaded,
            trees_state(),
            StateError::NotDirty("n".into()),
        ),
        (
            "a node only L holds that is not dirty",
            uploaded,
            trees_state(),
            StateError::NotDirty("n".into()),
        ),
        (
            "a parked node that is not dirty",
            trees(),
            TreesState {
                dirty: Default::default(),
                unsearched: Default::default(),
                ..trees_state()
            },
            StateError::NotDirty("b".into()),
        ),
        (
            "an unsearched node that is not dirty",
            trees(),
            broken(trees_state(), |state| {
                state.unsearched.insert("a".into());
            }),
            StateError::UnsearchedNotDirty("a".into()),
        ),
        (
            "a parked name for a placed node",
            trees(),
            broken(trees_state(), |state| {
                state.parked_as.insert("a".into(), "z".into());
            }),
            StateError::NotParked("a".into()),
        ),
        (
            "a node trashed here that R holds live",
            trees(),
            broken(trees_state(), |state| {
                state.trashed_here.insert("a".into(), 5);
            }),
            StateError::NotTrashedHere("a".into()),
        ),
        (
            "a node trashed here that R holds trashed by another commit",
            trees(),
            broken(trees_state(), |state| {
                state.trashed_here.insert("t".into(), 4);
            }),
            StateError::NotTrashedHere("t".into()),
        ),
        (
            "a node removed here that R holds live",
            trees(),
            broken(trees_state(), |state| {
                state.removed_here.insert("a".into(), 5);
            }),
            StateError::NotRemovedHere("a".into()),
        ),
        (
            "a node removed here that R holds trashed by another commit",
            trees(),
            broken(trees_state(), |state| {
                state.removed_here.insert("t".into(), 4);
            }),
            StateError::NotRemovedHere("t".into()),
        ),
        (
            "a node recorded in an ignored folder that L does not hold",
            trees(),
            broken(trees_state(), |state| {
                state.outside.insert("t".into());
            }),
            StateError::NotIgnoredLocal("t".into()),
        ),
        (
            "a folder the disk lost that is not recorded",
            trees(),
            broken(trees_state(), |state| {
                state.lost.insert("b".into());
            }),
            StateError::NotIgnoredLocal("b".into()),
        ),
        (
            "a displaced node that L holds",
            trees(),
            broken(trees_state(), |state| {
                state.displaced.insert("a".into(), held);
            }),
            StateError::NotDisplaced("a".into()),
        ),
        (
            "a displaced node that R holds trashed",
            trees(),
            broken(trees_state(), |state| {
                state.displaced.insert("t".into(), held);
            }),
            StateError::NotDisplaced("t".into()),
        ),
        (
            "a file edited here that R holds trashed",
            trees(),
            broken(trees_state(), |state| {
                state.edited_here.insert("t".into(), 1);
            }),
            StateError::NotEditedHere("t".into()),
        ),
        (
            "a file edited here whose content R holds from another commit",
            trees(),
            broken(trees_state(), |state| {
                state.edited_here.insert("a".into(), 2);
            }),
            StateError::NotEditedHere("a".into()),
        ),
        (
            "versions replaced in a node R does not hold",
            trees(),
            broken(trees_state(), |state| {
                state
                    .replaced_versions
                    .insert("gone".into(), vec!["v1".into()]);
            }),
            StateError::NotRemote("gone".into()),
        ),
        (
            "earlier records of a node R does not hold",
            trees(),
            broken(trees_state(), |state| {
                let past = state.past_positions["a"].clone();
                state.past_positions.insert("gone".into(), past);
            }),
            StateError::NotPastPosition("gone".into()),
        ),
        (
            "a node moved here that R holds at a revision before that commit",
            trees(),
            broken(trees_state(), |state| {
                let moved = MovedHere {
                    seq: 2,
                    upgraded: false,
                };
                state.moved_here.insert("a".into(), moved);
            }),
            StateError::NotMovedHere("a".into()),
        ),
    ];
    for (name, mut trees, state, error) in cases {
        assert_eq!(trees.restore(state), Err(error), "{name}");
    }
}

#[test]
fn commits_restore_a_valid_state_and_refuse_each_broken_one() {
    let restored = Commits::from_state(commits_state()).map(|commits| commits.state());
    assert_eq!(restored, Ok(commits_state()));

    let flight = |change: fn(&mut InFlight)| {
        broken(commits_state(), |state| {
            change(state.in_flight.as_mut().unwrap());
        })
    };
    let cases = [
        (
            "a commit in flight that does not take the next slot",
            CommitsState {
                accepted: 6,
                ..commits_state()
            },
            StateError::DeviceSeq {
                accepted: 6,
                device_seq: 6,
            },
        ),
        (
            "a result with an outcome missing",
            flight(|flight| {
                flight.result.as_mut().unwrap().results.pop();
            }),
            StateError::ResultMismatch,
        ),
        (
            "planned deletes for a moved node",
            flight(|flight| {
                flight.deletes.insert("m".into(), vec!["m".into()]);
            }),
            StateError::DeleteWithoutOp("m".into()),
        ),
        (
            "a cap of 0",
            CommitsState {
                cap: Some(0),
                ..commits_state()
            },
            StateError::Cap(0),
        ),
        (
            "a cap of MAX_OPS",
            CommitsState {
                cap: Some(MAX_OPS),
                ..commits_state()
            },
            StateError::Cap(MAX_OPS),
        ),
        (
            "a folder merged into itself",
            broken(commits_state(), |state| {
                state.aliases.insert("h".into(), "h".into());
            }),
            StateError::SelfAlias("h".into()),
        ),
        (
            "a Restore op in flight that no queued restore asks for",
            broken(commits_state(), |state| {
                state.restores.remove("r1");
            }),
            StateError::RestoreNotQueued(2),
        ),
        (
            "a restore that ended and is still queued",
            broken(commits_state(), |state| {
                state.restores.insert("r0".into(), restore(3));
            }),
            StateError::RestoreResolved("r0".into()),
        ),
        (
            "a restore with two outcomes",
            broken(commits_state(), |state| {
                state.resolved.push(state.resolved[0].clone());
            }),
            StateError::RestoreResolved("r0".into()),
        ),
    ];
    for (name, state, error) in cases {
        assert_eq!(Commits::from_state(state), Err(error), "{name}");
    }
}

#[test]
fn effects_restore_a_valid_state_and_refuse_each_broken_one() {
    let restored = Effects::from_state(effects_state()).map(|effects| effects.state());
    assert_eq!(restored, Ok(effects_state()));

    let mut state = effects_state();
    state.cleared = 4;
    let error = StateError::EffectCounter {
        cleared: 4,
        counter: 4,
    };
    assert_eq!(Effects::from_state(state), Err(error));
}

/// An entry on a filesystem that reports no birth time keeps an unknown birth time through a
/// restore, and its identity never equals one with a known birth time.
#[test]
fn an_identity_without_a_birth_time_round_trips() {
    let unknown = FileIdentity {
        dev: 1,
        ino: 7,
        birth_ns: None,
    };
    assert_ne!(
        unknown,
        FileIdentity {
            birth_ns: Some(0),
            ..unknown
        }
    );
    let shape = dir(ROOT, "u");
    let mut trees = Trees::new(ROOT);
    trees.set_remote("u".into(), remote(shape.clone()));
    trees.set_local(
        "u".into(),
        LocalNode {
            shape: shape.clone(),
            identity: unknown,
            parked: false,
        },
    );
    trees.set_synced("u".into(), Some(shape));
    let mut loaded = Trees::new(ROOT);
    for (id, node) in trees.remote().iter() {
        loaded.set_remote(id.clone(), node.clone());
    }
    for (id, node) in trees.local().iter() {
        loaded.set_local(id.clone(), node.clone());
    }
    for (id, shape) in trees.synced().iter() {
        loaded.set_synced(id.clone(), Some(shape.clone()));
    }
    assert_eq!(loaded.restore(trees.state()), Ok(()));
    loaded.rules_scanned();
    assert_eq!(loaded, trees);
    assert_eq!(
        loaded.local().get("u").map(|node| node.identity),
        Some(unknown)
    );

    let state = EffectsState {
        in_flight: vec![EffectRecord {
            effect_id: "effect".into(),
            node: "u".into(),
            counter: 1,
            effect: Effect::Move {
                from: Location::Parked("u".into()),
                identity: unknown,
                to: Location::Parked("v".into()),
            },
            after: None,
        }],
        cleared: 0,
        failed: [(
            "w".into(),
            Effect::RemoveDir {
                at: Location::Parked("w".into()),
                identity: unknown,
            },
        )]
        .into(),
    };
    let restored = Effects::from_state(state.clone()).map(|effects| effects.state());
    assert_eq!(restored, Ok(state));
}
