// SPDX-License-Identifier: Apache-2.0

/// The person edits a file and then restores an earlier version of it, before the edit was sent.
/// The edit goes first and becomes a version, the earlier version becomes current, and the file
/// keeps its node. Nothing is a conflict copy.
#[test]
fn an_edited_file_restored_to_an_earlier_version_keeps_the_edit_as_a_version() {
    let seed = [act(1, 0, 0, 3, 0)];
    let remote_edit = [act(2, 0, 0, 0, 1)];
    // Versions are listed newest first, so name 1 picks the first of the two.
    let edit_then_restore = (true, vec![act(2, 0, 0, 0, 2), act(6, 0, 0, 1, 0)]);
    for cut in CUTS {
        let during = [edit_then_restore.clone()];
        let ran = run_cut(&seed, &[], &remote_edit, MAX_OPS, false, &during, cut).unwrap();
        let files = nodes_with(&ran.space, 1, Some(2));
        assert_eq!(files.len(), 1, "{cut:?}");
        assert_eq!(ran.resolved[0].outcome, Outcome::Restored, "{cut:?}");
        let digest = |device, text| content(device, text).manifest_digest;
        assert_eq!(
            version_digests(&ran.space, &files[0]),
            [digest(0, 0), digest(1, 2), digest(2, 1), digest(0, 0)],
            "{cut:?}"
        );
    }
}

/// The response to a commit that carries a restore is lost. The device sends the same commit
/// again, and the server lists exactly one new version for the restore.
#[test]
fn a_restore_whose_response_was_lost_is_applied_once() {
    let seed = [act(1, 0, 0, 3, 0)];
    let remote_edit = [act(2, 0, 0, 0, 1)];
    let restore = (true, vec![act(6, 0, 0, 1, 0)]);
    let ran = run_cut(
        &seed,
        &[],
        &remote_edit,
        MAX_OPS,
        false,
        &[restore],
        Cut::Applied,
    )
    .unwrap();
    let file = &nodes_with(&ran.space, 1, Some(2))[0];
    assert_eq!(version_digests(&ran.space, file).len(), 3);
    assert_eq!(ran.resolved.len(), 1);
    assert_eq!(ran.resolved[0].outcome, Outcome::Restored);
}

/// A version that expired on the server, or was never there, cannot be restored. The request ends
/// as rejected, and a file created on this device meanwhile is uploaded in the same commit.
#[test]
fn a_restore_of_a_version_the_server_does_not_have_is_refused_and_sync_goes_on() {
    let seed = [act(1, 0, 0, 3, 0)];
    let create_and_restore = (true, vec![act(1, 0, 0, 0, 1), act(6, 0, 0, 0, 3)]);
    for cut in CUTS {
        let during = [create_and_restore.clone()];
        let ran = run_cut(&seed, &[], &[], MAX_OPS, false, &during, cut).unwrap();
        let rejected = Outcome::Rejected(Rejection::NotFound);
        assert_eq!(ran.resolved.len(), 1, "{cut:?}");
        assert_eq!(ran.resolved[0].outcome, rejected, "{cut:?}");
        assert_eq!(nodes_with(&ran.space, 1, Some(2)).len(), 2, "{cut:?}");
    }
}

/// The store is replaced by a copy from before a restore was sent, after the server applied it.
/// The server's count of this device's commits is then one ahead of the store, which no healthy
/// store shows. The space enters recovery and the restore, still queued in the old copy, is
/// neither sent again nor dropped.
#[test]
fn a_store_rolled_back_past_a_sent_restore_starts_recovery_and_sends_nothing() {
    let (mut space, mut other) = seeded(&a_folder_with_a_file());
    remote_action(&mut space, &mut other, 2, &act(4, 0, 0, 0, 0));
    let trashed = nodes_with(&space, 2, None);
    let mut trees = Trees::new(space.space_id().to_owned());
    for state in space.nodes().iter().skip(1) {
        let (id, node) = RemoteNode::from_state(state).unwrap();
        trees.set_remote(id, node);
    }
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    let intent = RestoreIntent::from_trash(trashed[0].clone(), None, None).unwrap();
    commits.restore(ids.uuid_v7(NOW as u64), intent.clone());
    let backup = commits.state();

    let (history, head) = (space.history_id().to_owned(), space.head_seq());
    let commit = commits.build(&mut trees, &mut ids, NOW as u64, &history, "me", head);
    let me = session("me");
    space.commit(&me, NOW, &commit.cloned().unwrap()).unwrap();
    let head = space.head_seq();

    let mut commits = Commits::from_state(backup).unwrap();
    assert_eq!(commits.connected(&space.welcome(&me)), Reconnect::Recovery);
    for _ in 0..2 {
        assert!(matches!(commits.halted(), Some(Halt::Recovery(_))));
        assert_eq!(commits.restores().values().collect::<Vec<_>>(), [&intent]);
        let built = commits.build(&mut trees, &mut ids, NOW as u64, &history, "me", head);
        assert_eq!(built, None);
        commits = Commits::from_state(commits.state()).unwrap();
    }
    assert_eq!(space.head_seq(), head);
}

/// A minimal interleaving the property found. The user moves and edits a file this device
/// created, and the other device deletes it. A commit that fits two ops carries the restore of
/// another file's version and the move, which revives the file with the content the server
/// already acknowledged (R8). The edit follows in the next commit. What the move revived is no
/// restored write: the edit replaces it, and it stays a version of the file.
#[test]
fn content_a_move_revives_next_to_a_restore_is_replaced_by_the_unsent_edit() {
    let seed = [act(1, 0, 0, 0, 0), act(0, 0, 0, 3, 0)];
    let local = [
        act(4, 652086880384956978, 0, 0, 0),
        act(1, 0, 9536159111846681508, 2, 0),
        act(0, 0, 0, 5, 0),
    ];
    let remote = [act(1, 0, 2753156350273366001, 0, 0)];
    let during = [
        (false, vec![act(5, 0, 0, 0, 0)]),
        (
            true,
            vec![
                act(0, 0, 7165637464455973765, 0, 0),
                act(3, 3958708204467096970, 3613361925314318621, 3, 0),
            ],
        ),
        (
            true,
            vec![
                act(2, 455309143188881385, 0, 0, 1),
                act(6, 3741528846974593209, 0, 0, 0),
            ],
        ),
        (
            false,
            vec![
                act(1, 0, 4063425664772464998, 0, 0),
                act(4, 2784706346093506431, 0, 0, 0),
                act(1, 0, 0, 0, 0),
            ],
        ),
        (true, vec![act(6, 206315280794903461, 0, 0, 0)]),
    ];
    let ran = run_cut(&seed, &local, &remote, 2, false, &during, Cut::None).unwrap();
    let mut files = ran.space.nodes().into_iter().filter(|n| n.kind == 2);
    let moved = files.find(|n| n.name == NAMES[3]);
    let digest = |text| content(1, text).manifest_digest;
    assert_eq!(
        version_digests(&ran.space, &moved.unwrap().node_id),
        [digest(1), digest(0)]
    );
}

/// A minimal interleaving the property found. This device creates the file b, and its S update
/// waits behind a delete and a move that are still unsent, so the file is left out of planning.
/// The user edits b and then asks for its first version. One op fits a commit. The restore
/// waits until the edit is sent, so the first version is current at the end and the edit is a
/// version under it.
#[test]
fn a_version_restore_follows_an_edit_of_a_file_left_out_of_planning() {
    let seed = [
        act(0, 0, 0, 0, 0),
        act(0, 0, 2960225115312676489, 0, 0),
        act(1, 0, 482761575999273747, 2, 0),
    ];
    let local = [
        act(4, 3338552224821716477, 0, 0, 0),
        act(3, 4169982272558885297, 686010811002822325, 0, 0),
        act(1, 0, 10528470701804065178, 2, 1),
    ];
    let during = [
        (false, vec![act(3, 2253923601960195439, 0, 3, 0)]),
        (true, vec![act(2, 6132494219199068644, 0, 0, 0)]),
        (true, vec![act(6, 7039772437741740600, 0, 0, 0)]),
    ];
    for cut in CUTS {
        let ran = run_cut(&seed, &local, &[], 1, false, &during, cut).unwrap();
        assert_eq!(ran.resolved.len(), 1, "{cut:?}");
        assert_eq!(ran.resolved[0].outcome, Outcome::Restored, "{cut:?}");
        let node = ran.resolved[0].intent.node();
        let digest = |text| content(1, text).manifest_digest;
        assert_eq!(
            version_digests(&ran.space, node),
            [digest(1), digest(0), digest(1)],
            "{cut:?}"
        );
    }
}

/// A minimal interleaving the property found. The other device deletes a folder, and the user
/// edits a file in it before the delete reaches the disk and then asks for the folder back from
/// the trash. One op fits a commit, so the restore brings the file back with its old content
/// first and the edit follows. The edit is current at the end, as an edit beats a delete, and
/// the content the restore brought back is the version under it.
#[test]
fn an_unsent_edit_of_a_file_in_a_restored_folder_stays_current() {
    let seed = [
        act(0, 0, 0, 0, 0),
        act(0, 0, 101854363273155071, 2, 0),
        act(0, 0, 3503316990192837406, 0, 0),
        act(1, 0, 2272586388003352129, 0, 0),
        act(1, 0, 9015892922196451787, 2, 0),
        act(1, 0, 110837118209456033, 0, 0),
        act(0, 0, 11223625915557445946, 0, 0),
        act(1, 0, 12002327795321761196, 0, 0),
        act(1, 0, 5987160327725455170, 0, 0),
        act(0, 0, 5314422076761796984, 0, 0),
    ];
    let local = [
        act(4, 989225936368150641, 0, 0, 0),
        act(0, 0, 0, 0, 0),
        act(4, 1897456816994156894, 0, 0, 0),
    ];
    let remote = [act(2, 11371124727216740042, 0, 0, 0)];
    let during = [
        (false, vec![act(4, 6116793111143655890, 0, 0, 0)]),
        (
            true,
            vec![
                act(2, 16490628292596182229, 0, 0, 0),
                act(5, 13447724379140298103, 0, 0, 0),
            ],
        ),
    ];
    for cut in CUTS {
        let ran = run_cut(&seed, &local, &remote, 1, true, &during, cut).unwrap();
        assert_eq!(ran.resolved.len(), 1, "{cut:?}");
        assert_eq!(ran.resolved[0].outcome, Outcome::Restored, "{cut:?}");
        let folder = ran.resolved[0].intent.node();
        let mut inside = ran
            .space
            .nodes()
            .into_iter()
            .filter(|n| n.parent_id == folder);
        let file = inside.find(|n| n.kind == 2).unwrap();
        let versions = version_digests(&ran.space, &file.node_id);
        let (edit, restored) = (content(1, 0), content(2, 0));
        assert_eq!(
            versions[..2],
            [edit.manifest_digest, restored.manifest_digest],
            "{cut:?}"
        );
    }
}
