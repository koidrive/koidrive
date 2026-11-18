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

/// A space whose tree another device made, and this device with that tree in L, S and R.
struct Bench {
    space: SpaceDo,
    trees: Trees,
    commits: Commits,
    ids: IdGen,
    other: IdGen,
    last_seq: u64,
    /// Where each commit is cut once, as [`run_cut`] cuts a commit that carries a restore.
    cut: Cut,
    cut_commits: BTreeSet<String>,
    /// How many of the next commits the server refuses with `INTERNAL` before it reads them.
    refused: usize,
}

impl Bench {
    /// Folders `path` nested in each other under the root, with `count` files of 4,096 chunks in the last.
    fn large_files(path: &[&str], count: usize) -> (Self, Vec<String>) {
        let (space, other) = seeded(&[]);
        let root = space.space_id().to_owned();
        let mut bench = Bench {
            trees: Trees::new(root.clone()),
            commits: Commits::default(),
            ids: IdGen::new(7),
            space,
            other,
            last_seq: 0,
            cut: Cut::None,
            cut_commits: BTreeSet::new(),
            refused: 0,
        };
        let mut folders = Vec::new();
        for name in path {
            let node_id = bench.other.uuid_v7(NOW as u64);
            let parent_id = folders.last().unwrap_or(&root).clone();
            bench.commit_by_other(op::Op::CreateDir(CreateDir {
                node_id: node_id.clone(),
                parent_id,
                name: (*name).to_owned(),
                pending_for: String::new(),
            }));
            folders.push(node_id);
        }
        let chunk = ChunkRef {
            hash: Sha256::digest(b"large").to_vec(),
            size: 3,
            encoding: 0,
        };
        let list: Vec<u8> = (0..4096)
            .flat_map(|_| chunk.hash.iter().copied().chain(chunk.size.to_le_bytes()))
            .collect();
        let content = Content {
            size: 3 * 4096,
            chunk_count: 4096,
            manifest_digest: Sha256::digest(list).to_vec(),
            chunks: vec![chunk; 4096],
            ..Default::default()
        };
        for i in 0..count {
            let node_id = bench.other.uuid_v7(NOW as u64);
            bench.commit_by_other(op::Op::CreateFile(CreateFile {
                node_id,
                parent_id: folders.last().unwrap().clone(),
                name: format!("f{i}.mov"),
                content: Some(content.clone()),
                ..Default::default()
            }));
        }
        let nodes = bench
            .space
            .nodes()
            .into_iter()
            .filter(|node| node.node_id != root);
        for (ino, state) in nodes.enumerate() {
            let (id, node) = RemoteNode::from_state(&state).unwrap();
            let local = LocalNode {
                left_out_when_placed: false,
                content_from_server: true,
                shape: node.shape.clone(),
                identity: identity(ino as u64),
                parked: false,
            };
            bench.trees.set_local(id.clone(), local);
            bench.trees.set_synced(id.clone(), Some(node.shape.clone()));
            bench.trees.set_remote(id, node);
        }
        bench.last_seq = bench.space.head_seq();
        rules().read(&mut bench.trees);
        bench.trees.settle(bench.commits.aliases());
        (bench, folders)
    }

    fn commit_by_other(&mut self, body: op::Op) {
        let session = session("other");
        let ops = vec![Op {
            op: Some(body),
            ..Default::default()
        }];
        let mut unique = ops.clone();
        if let Some(op::Op::CreateFile(create)) = &mut unique[0].op
            && let Some(content) = &mut create.content
        {
            content.chunks.dedup();
        }
        let receipts = upload(&mut self.space, &session, &unique);
        let commit = Commit {
            commit_id: self.other.uuid_v7(NOW as u64),
            base_seq: self.space.head_seq(),
            ops,
            receipts,
            history_id: self.space.history_id().to_owned(),
            device_seq: self.space.device_seq(&session.device_id) + 1,
            ..Default::default()
        };
        self.space.commit(&session, NOW, &commit).unwrap();
    }

    fn pull(&mut self) {
        let resume = Resume {
            history_id: self.space.history_id().to_owned(),
            since_seq: self.last_seq,
            ..Default::default()
        };
        for record in self.space.resume(&resume).unwrap() {
            apply(&mut self.trees, &record.changes);
            self.last_seq = record.seq;
        }
        self.trees.settle(self.commits.aliases());
    }

    fn restart(&mut self) {
        let effects = Effects::default();
        let (trees, commits, _) = reload(&self.trees, &self.commits, &effects, &rules()).unwrap();
        (self.trees, self.commits) = (trees, commits);
        self.commits.connected(&self.space.welcome(&session("me")));
    }

    /// Sends what this device commits until it has nothing left, agreeing to a deletion it asks
    /// about, and calls `between` after each commit the server applied.
    fn sync(&mut self, mut between: impl FnMut(&mut Bench)) {
        let me = session("me");
        for _ in 0..STEPS {
            self.pull();
            let in_flight = self.commits.in_flight().map(|flight| flight.commit.clone());
            let history = self.space.history_id().to_owned();
            let built = in_flight.or_else(|| {
                let base = self.last_seq;
                (self.commits.build(
                    &mut self.trees,
                    &mut self.ids,
                    NOW as u64,
                    &history,
                    "me",
                    base,
                ))
                .cloned()
            });
            let Some(commit) = built else {
                let asked = (self.commits.questions().iter())
                    .find(|question| matches!(question.kind, QuestionKind::MassDelete { .. }))
                    .map(|question| question.id.clone());
                match asked {
                    Some(id) => assert!(self.commits.decide(&id, Answer::Delete)),
                    None => return,
                }
                continue;
            };
            let cut = match self.cut_commits.insert(commit.commit_id.clone()) {
                true => self.cut,
                false => Cut::None,
            };
            if cut == Cut::Built {
                self.restart();
            }
            if self.refused > 0 {
                self.refused -= 1;
                let refusal = Error {
                    code: error::Code::Internal as i32,
                    ..Default::default()
                };
                assert_eq!(
                    self.commits.failed(&mut self.trees, &refusal),
                    Ok(Failure::Resend)
                );
                continue;
            }
            let receipts = upload(&mut self.space, &me, &commit.ops);
            let request = Commit { receipts, ..commit };
            let committed = match self.space.commit(&me, NOW, &request) {
                Ok(committed) => committed,
                Err(error) => {
                    self.commits.failed(&mut self.trees, &error).unwrap();
                    continue;
                }
            };
            if cut == Cut::Applied {
                self.restart();
                continue;
            }
            self.commits.received(committed.result).unwrap();
            self.pull();
            assert!(self.commits.acknowledge(&mut self.trees, self.last_seq));
            self.trees.settle(self.commits.aliases());
            between(self);
        }
        panic!("no end within {STEPS} steps");
    }

    /// Removes `folder` with all it holds from L, and sends the deletion.
    fn delete(&mut self, folder: &str, between: impl FnMut(&mut Bench)) {
        let mut below = vec![folder.to_owned()];
        while let Some(id) = below.pop() {
            below.extend(self.trees.local_children(&id).cloned());
            self.trees.remove_local(&id);
        }
        self.trees.settle(self.commits.aliases());
        self.sync(between);
        let trashed = |node: &NodeState| node.node_id == folder && node.status == 2;
        assert!(self.space.nodes().iter().any(trashed));
    }

    /// Asks for `node` back from the trash, and sends the restore.
    fn restore(&mut self, node: &str, between: impl FnMut(&mut Bench)) {
        let intent = RestoreIntent::from_trash(node.to_owned(), None, None).unwrap();
        assert!(self.commits.restore(self.ids.uuid_v7(NOW as u64), intent));
        self.sync(between);
    }

    fn live_files(&self) -> usize {
        nodes_with(&self.space, 1, Some(2)).len()
    }

    fn outcomes(&self) -> Vec<Outcome> {
        (self.commits.resolved().iter())
            .map(|resolved| resolved.outcome.clone())
            .collect()
    }
}

fn rules() -> Rules {
    Rules {
        file: Vec::new(),
        exclude: Vec::new(),
        include: Vec::new(),
    }
}

/// A folder of eight files of 4,096 chunks each is too large to delete in one op, and to restore in
/// one commit. The person deletes it and asks for it back, and every file comes back.
#[test]
fn a_restored_folder_of_large_files_brings_back_every_file() {
    let (mut bench, folders) = Bench::large_files(&["big"], 8);
    bench.delete(&folders[0], |_| {});
    assert_eq!(bench.live_files(), 0);
    bench.restore(&folders[0], |_| {});
    assert_eq!(bench.live_files(), 8);
    assert_eq!(bench.outcomes(), [Outcome::Restored]);
}

/// The person asks twice for the folder back before the first commit leaves. Both requests end
/// restored once every file is back.
#[test]
fn two_requests_for_a_big_folder_bring_back_every_file() {
    let (mut bench, folders) = Bench::large_files(&["big"], 8);
    bench.delete(&folders[0], |_| {});
    let intent = RestoreIntent::from_trash(folders[0].clone(), None, None).unwrap();
    let id = bench.ids.uuid_v7(NOW as u64);
    assert!(bench.commits.restore(id, intent));
    let ended_with = |bench: &mut Bench| {
        if !bench.outcomes().is_empty() {
            assert_eq!(bench.live_files(), 8);
        }
    };
    bench.restore(&folders[0], ended_with);
    assert_eq!(bench.outcomes(), [Outcome::Restored, Outcome::Restored]);
}

/// The device stops after each commit of the deletion and of the restore, so each continues from
/// the store.
#[test]
fn a_big_deletion_and_its_restore_continue_after_a_stop_between_commits() {
    let (mut bench, folders) = Bench::large_files(&["big"], 8);
    bench.delete(&folders[0], Bench::restart);
    bench.restore(&folders[0], Bench::restart);
    assert_eq!(bench.live_files(), 8);
    assert_eq!(bench.outcomes(), [Outcome::Restored]);
}

/// Every commit of the deletion and the restore is cut once: after it was saved and before it
/// left, or after the server applied it and before its response arrived.
#[test]
fn a_big_deletion_and_its_restore_cut_in_every_commit_bring_back_every_file() {
    for cut in [Cut::Built, Cut::Applied] {
        let (mut bench, folders) = Bench::large_files(&["big"], 8);
        bench.cut = cut;
        bench.delete(&folders[0], |_| {});
        bench.restore(&folders[0], |_| {});
        assert_eq!(bench.live_files(), 8, "{cut:?}");
        assert_eq!(bench.outcomes(), [Outcome::Restored], "{cut:?}");
    }
}

/// The server fails the first two commits of the deletion and of the restore, and the device sends
/// them again.
#[test]
fn a_big_restore_goes_on_after_the_server_fails_its_commits() {
    let (mut bench, folders) = Bench::large_files(&["big"], 8);
    bench.refused = 2;
    bench.delete(&folders[0], |_| {});
    bench.refused = 2;
    bench.restore(&folders[0], |bench| bench.refused = 1);
    assert_eq!(bench.live_files(), 8);
    assert_eq!(bench.outcomes(), [Outcome::Restored]);
}

/// Another device renames the folder above the trashed one between the commits of its deletion
/// and between those of its restore. Every file comes back, in the renamed folder.
#[test]
fn a_big_restore_brings_back_every_file_after_its_parent_is_renamed() {
    let (mut bench, folders) = Bench::large_files(&["outer", "big"], 8);
    let mut renames = 0;
    let mut rename = |bench: &mut Bench| {
        renames += 1;
        let root = bench.space.space_id().to_owned();
        bench.commit_by_other(op::Op::Move(Move {
            node_id: folders[0].clone(),
            new_parent_id: root,
            new_name: format!("outer {renames}"),
            from: None,
            pending_for: String::new(),
        }));
    };
    bench.delete(&folders[1], &mut rename);
    bench.restore(&folders[1], &mut rename);
    assert_eq!(bench.live_files(), 8);
    assert_eq!(bench.outcomes(), [Outcome::Restored]);
    let big = (bench.space.nodes().into_iter()).find(|node| node.node_id == folders[1]);
    assert_eq!(big.unwrap().parent_id, folders[0]);
}
