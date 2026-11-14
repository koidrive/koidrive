// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_folder_brought_back_at_its_pending_name_takes_its_own_name() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("a"), b"the file");
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    fs::rename(site.root.join("a"), site.root.join("x")).unwrap();
    fs::create_dir(site.root.join("a")).unwrap();
    fs::rename(site.root.join("x"), site.root.join("a/child")).unwrap();
    b.scan_until(|b| !b.work.is_empty());
    let Some(Work::Send(commit)) = b.work.pop_front() else {
        panic!("a commit is sent");
    };
    let result = remote.commit(&b.caller, &commit).unwrap();
    fs::remove_dir_all(site.root.join("a")).unwrap();
    b.settle();
    a.pull(&remote);
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    let child = files(&site_a.root).into_iter().next().unwrap().0;
    a.create(&mut remote, &site_a.root.join(child), b"edited remotely");
    b.pull(&remote);
    let step = b.folder.sent(Ok(result));
    b.take(step).unwrap();
    for _ in 0..6 {
        b.run(&mut remote);
        b.settle();
    }
    assert_eq!(
        files(&site.root),
        [("a/child".to_owned(), "edited remotely".to_owned())]
    );
}

#[test]
fn a_file_sent_aside_in_a_split_swap_and_deleted_in_flight_takes_its_name() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("a"), b"file A");
    a.create(&mut remote, &site_a.root.join("b"), b"file B");
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    for (from, to) in [("a", "tmp"), ("b", "a"), ("tmp", "b")] {
        fs::rename(site.root.join(from), site.root.join(to)).unwrap();
    }
    b.scan_until(|b| !b.work.is_empty());
    let Some(Work::Send(_)) = b.work.pop_front() else {
        panic!("the swap is sent");
    };
    let refusal = koidrive_proto::v1::Error {
        code: koidrive_proto::v1::error::Code::LimitExceeded as i32,
        ..Default::default()
    };
    let step = b.folder.sent(Err(CallError::Server(ServerError(refusal))));
    b.take(step).unwrap();
    b.scan_until(|b| !b.work.is_empty());
    let Some(Work::Send(split)) = b.work.pop_front() else {
        panic!("the split swap is sent");
    };
    let moves: Vec<String> = (split.ops.iter())
        .filter_map(|op| match &op.op {
            Some(op::Op::Move(moved)) => Some(moved.new_name.clone()),
            _ => None,
        })
        .collect();
    let [aside] = &moves[..] else {
        panic!("one move aside: {moves:?}");
    };
    let target = aside.strip_suffix(" (pending)").unwrap().to_owned();
    let result = remote.commit(&b.caller, &split).unwrap();
    fs::remove_file(site.root.join(&target)).unwrap();
    b.settle();
    a.pull(&remote);
    a.rest(&mut remote);
    fs::write(site_a.root.join(aside), b"edited remotely").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.pull(&remote);
    let step = b.folder.sent(Ok(result));
    b.take(step).unwrap();
    sync(&mut remote, (&site_a, &mut a), (&site, &mut b));
    assert!(remote.names().iter().all(|name| !name.contains("(pending")));
    assert!(files(&site.root).contains(&(target, "edited remotely".to_owned())));
}

#[test]
fn a_refusal_handled_after_a_disconnect_does_not_hold_a_remote_rename() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("x"), b"kept");
    a.settle();
    let (site_b, mut b) = linked(&mut remote, "b");
    fs::rename(site.root.join("x"), site.root.join("sent")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert!(matches!(a.work.pop_front(), Some(Work::Send(_))));
    assert!(a.work.is_empty());
    fs::rename(site.root.join("sent"), site.root.join("x")).unwrap();
    a.scan_until(|a| {
        a.folder
            .engine
            .trees
            .local()
            .iter()
            .any(|(_, n)| n.shape.name == "x")
    });
    fs::rename(site_b.root.join("x"), site_b.root.join("remote")).unwrap();
    b.scan_until(|b| !b.work.is_empty());
    b.run(&mut remote);
    assert_eq!(remote.names(), ["remote"]);
    a.pull(&remote);
    a.event(Event::Disconnected {
        reason: koidrive_net::DisconnectReason::Timeout,
        retry_in: Duration::from_secs(1),
    })
    .unwrap();
    let step = a.folder.sent(Err(CallError::Server(ServerError(ProtoError {
        code: Code::ReadOnly as i32,
        ..Default::default()
    }))));
    a.take(step).unwrap();
    assert!(a.folder.engine.commits.in_flight().is_none());
    assert!(a.folder.offline);
    a.run(&mut remote);
    assert_eq!(names(&site.root), ["remote"]);
}

#[test]
fn an_upload_abandoned_after_a_disconnect_does_not_hold_a_remote_rename() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("x"), b"kept");
    a.create(&mut remote, &site.root.join("other"), b"old");
    a.settle();
    let (site_b, mut b) = linked(&mut remote, "b");
    fs::rename(site.root.join("x"), site.root.join("sent")).unwrap();
    fs::write(site.root.join("other"), b"new").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert!(matches!(a.work.pop_front(), Some(Work::Upload(_))));
    assert!(a.work.is_empty());
    fs::rename(site.root.join("sent"), site.root.join("x")).unwrap();
    a.scan_until(|a| {
        a.folder
            .engine
            .trees
            .local()
            .iter()
            .any(|(_, n)| n.shape.name == "x")
    });
    fs::rename(site_b.root.join("x"), site_b.root.join("remote")).unwrap();
    b.scan_until(|b| !b.work.is_empty());
    b.run(&mut remote);
    a.pull(&remote);
    a.event(Event::Disconnected {
        reason: koidrive_net::DisconnectReason::Timeout,
        retry_in: Duration::from_secs(1),
    })
    .unwrap();
    let step = a
        .folder
        .uploaded(Err(ChunkError::SourceChanged { hash: vec![0; 32] }));
    a.take(step).unwrap();
    assert!(a.folder.engine.commits.in_flight().is_none());
    assert!(a.folder.offline);
    a.run(&mut remote);
    assert_eq!(names(&site.root), ["other", "remote"]);
}

/// A file replaced by a hard link to another file takes that file's content and keeps its own node, so the other
/// path stays on the server.
#[test]
fn a_file_replaced_by_a_hard_link_to_another_leaves_the_other_on_the_server() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("one.txt"), b"one");
    a.create(&mut remote, &site_a.root.join("two.txt"), b"two");
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    fs::remove_file(site.root.join("two.txt")).unwrap();
    fs::hard_link(site.root.join("one.txt"), site.root.join("two.txt")).unwrap();
    b.scan_until(|b| !b.work.is_empty());
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert_eq!(remote.names(), ["one.txt", "two.txt"]);
    assert_eq!(names(&site.root), ["one.txt", "two.txt"]);
}

/// Device b makes a folder and moves a file it downloaded into it before its observer looks. The scan lists the move
/// before the new folder, and the move still reaches the server and device a.
#[test]
fn a_file_moved_into_a_folder_made_in_the_same_batch_reaches_the_other_device() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("report.txt"), b"the report");
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    fs::create_dir(site.root.join("docs")).unwrap();
    fs::rename(
        site.root.join("report.txt"),
        site.root.join("docs/renamed.txt"),
    )
    .unwrap();
    sync(&mut remote, (&site_a, &mut a), (&site, &mut b));
    assert_eq!(remote.names(), ["docs", "renamed.txt"]);
    assert_eq!(
        files(&site_a.root),
        [("docs/renamed.txt".to_owned(), "the report".to_owned())]
    );
}

/// Device b moves the file `a` into a new folder and renames the folder `b` to `a` before its observer looks. The folder
/// takes the name only after the file left it, so the file is not lost.
#[test]
fn a_folder_renamed_to_the_name_a_file_left_for_a_new_folder_keeps_the_file() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("a"), b"the file");
    fs::create_dir(site_a.root.join("b")).unwrap();
    a.create(&mut remote, &site_a.root.join("b/f"), b"in b");
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    fs::create_dir(site.root.join("docs")).unwrap();
    fs::rename(site.root.join("a"), site.root.join("docs/a")).unwrap();
    fs::rename(site.root.join("b"), site.root.join("a")).unwrap();
    sync(&mut remote, (&site_a, &mut a), (&site, &mut b));
    assert_eq!(remote.names(), ["a", "a", "docs", "f"]);
    let both = [("a/f", "in b"), ("docs/a", "the file")]
        .map(|(path, text)| (path.to_owned(), text.to_owned()));
    assert_eq!(files(&site_a.root), both);
}

/// Device b renames the file `a` to `x`, makes a folder `a` and moves `x` into it as `a/ß` before its observer looks.
/// The scan reports the move of `a` into the folder that takes its name. L stays a tree that a restart can load. The
/// folder is created under a pending name while the file still holds its name, and takes its name once the file has
/// moved into it (design §2.6), so the file reaches the server and device a at `a/ß`.
#[test]
fn a_file_moved_into_a_new_folder_that_takes_its_name_in_one_batch_keeps_l_a_tree() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("a"), b"the file");
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    fs::rename(site.root.join("a"), site.root.join("x")).unwrap();
    fs::create_dir(site.root.join("a")).unwrap();
    fs::rename(site.root.join("x"), site.root.join("a/ß")).unwrap();
    sync(&mut remote, (&site_a, &mut a), (&site, &mut b));
    assert_eq!(remote.names(), ["a", "ß"]);
    let moved = [("a/ß".to_owned(), "the file".to_owned())];
    assert_eq!(files(&site_a.root), moved);
    assert_eq!(files(&site.root), moved);
}

/// Device b renames the file `a` to `x`, renames the folder `d` to `a` and moves `x` into the folder's subfolder as
/// `a/e/a` before its observer looks. The scan reports the file's move before the folder's, so the file waits for
/// the move that brings the folder above it, and it reaches the server and device a there.
#[test]
fn a_file_moved_below_a_folder_that_takes_its_name_in_one_batch_is_kept() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir_all(site_a.root.join("d/e")).unwrap();
    a.create(&mut remote, &site_a.root.join("a"), b"the file");
    a.create(&mut remote, &site_a.root.join("d/e/k"), b"k");
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    fs::rename(site.root.join("a"), site.root.join("x")).unwrap();
    fs::rename(site.root.join("d"), site.root.join("a")).unwrap();
    fs::rename(site.root.join("x"), site.root.join("a/e/a")).unwrap();
    sync(&mut remote, (&site_a, &mut a), (&site, &mut b));
    let both = [("a/e/a", "the file"), ("a/e/k", "k")]
        .map(|(path, text)| (path.to_owned(), text.to_owned()));
    assert_eq!(files(&site_a.root), both);
    assert_eq!(files(&site.root), both);
}

/// Device b renames the folder `a` to `x`, makes a new folder `a` and moves `x` into it as `a/s` before its observer
/// looks. The scan reports the old folder's move into the new one and the new folder's creation, so L stays a tree that
/// a restart can load, and the file reaches device a two levels down.
#[test]
fn a_folder_moved_into_a_new_folder_that_takes_its_name_in_one_batch_keeps_l_a_tree() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("a")).unwrap();
    a.create(&mut remote, &site_a.root.join("a/f"), b"the file");
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    fs::rename(site.root.join("a"), site.root.join("x")).unwrap();
    fs::create_dir(site.root.join("a")).unwrap();
    fs::rename(site.root.join("x"), site.root.join("a/s")).unwrap();
    sync(&mut remote, (&site_a, &mut a), (&site, &mut b));
    let moved = [("a/s/f".to_owned(), "the file".to_owned())];
    assert_eq!(files(&site_a.root), moved);
    assert_eq!(files(&site.root), moved);
}

/// Device b swaps the file `a` and the folder `b` through a third name before its observer looks. The scan reports
/// two renames that each take the other's name. The folder keeps its child, and the file keeps its bytes, under the
/// name the server gives one side of a name swap (design §2.6).
#[test]
fn a_file_and_a_folder_swapped_in_one_batch_keep_the_folders_child() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("a"), b"file A");
    fs::create_dir(site_a.root.join("b")).unwrap();
    a.create(
        &mut remote,
        &site_a.root.join("b/child"),
        b"directory child",
    );
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    fs::rename(site.root.join("a"), site.root.join("tmp")).unwrap();
    fs::rename(site.root.join("b"), site.root.join("a")).unwrap();
    fs::rename(site.root.join("tmp"), site.root.join("b")).unwrap();
    sync(&mut remote, (&site_a, &mut a), (&site, &mut b));
    let both = files(&site_a.root);
    assert_eq!(files(&site.root), both);
    assert!(both.contains(&("a/child".to_owned(), "directory child".to_owned())));
    assert_eq!(both.iter().filter(|(_, text)| text == "file A").count(), 1);
}

/// Device b rotates the file `a` and the folders `b` and `c` through a third name before its observer looks: `c`
/// takes `a`, `b` takes `c` and the file takes `b`. Each folder keeps its own child on both devices.
#[test]
fn a_file_and_two_folders_rotated_in_one_batch_keep_each_child_in_its_folder() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("a"), b"A");
    fs::create_dir(site_a.root.join("b")).unwrap();
    a.create(&mut remote, &site_a.root.join("b/one"), b"B child");
    fs::create_dir(site_a.root.join("c")).unwrap();
    a.create(&mut remote, &site_a.root.join("c/two"), b"C child");
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    for (from, to) in [("a", "tmp"), ("c", "a"), ("b", "c"), ("tmp", "b")] {
        fs::rename(site.root.join(from), site.root.join(to)).unwrap();
    }
    sync(&mut remote, (&site_a, &mut a), (&site, &mut b));
    let both = files(&site_a.root);
    assert_eq!(both, files(&site.root));
    assert!(both.contains(&("a/two".to_owned(), "C child".to_owned())));
    assert!(both.contains(&("c/one".to_owned(), "B child".to_owned())));
    assert_eq!(both.iter().filter(|(_, bytes)| bytes == "A").count(), 1);
}

/// Device b swaps two files, and two folders with a file each, through a third name before its observer looks.
/// Every file keeps its bytes at its new path on both devices.
#[test]
fn files_and_folders_swapped_in_one_batch_keep_every_byte() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("one"), b"first file");
    a.create(&mut remote, &site_a.root.join("two"), b"second file");
    fs::create_dir(site_a.root.join("x")).unwrap();
    a.create(&mut remote, &site_a.root.join("x/in-x"), b"in x");
    fs::create_dir(site_a.root.join("y")).unwrap();
    a.create(&mut remote, &site_a.root.join("y/in-y"), b"in y");
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    for (first, second) in [("one", "two"), ("x", "y")] {
        fs::rename(site.root.join(first), site.root.join("tmp")).unwrap();
        fs::rename(site.root.join(second), site.root.join(first)).unwrap();
        fs::rename(site.root.join("tmp"), site.root.join(second)).unwrap();
    }
    sync(&mut remote, (&site_a, &mut a), (&site, &mut b));
    let swapped = [
        ("one", "second file"),
        ("two", "first file"),
        ("x/in-y", "in y"),
        ("y/in-x", "in x"),
    ]
    .map(|(path, text)| (path.to_owned(), text.to_owned()));
    assert_eq!(files(&site_a.root), swapped);
    assert_eq!(files(&site.root), swapped);
}

#[test]
fn files_moved_between_two_swapped_folders_keep_their_paths_on_both_devices() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("a")).unwrap();
    fs::create_dir(site_a.root.join("b")).unwrap();
    a.create(&mut remote, &site_a.root.join("a/1"), b"one");
    a.create(&mut remote, &site_a.root.join("b/2"), b"two");
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    for (from, to) in [
        ("a", "t"),
        ("b", "a"),
        ("t", "b"),
        ("a/2", "b/2"),
        ("b/1", "a/1"),
    ] {
        fs::rename(site.root.join(from), site.root.join(to)).unwrap();
    }
    sync(&mut remote, (&site_a, &mut a), (&site, &mut b));
    let kept =
        [("a/1", "one"), ("b/2", "two")].map(|(path, text)| (path.to_owned(), text.to_owned()));
    assert_eq!(files(&site.root), kept);
    assert_eq!(files(&site_a.root), kept);
}

/// Device a swaps the file `a` with the file `b`, which its rules exclude, through a third name. The file that took
/// `b` left what this device syncs, so its copy stays on the server.
#[test]
fn a_file_swapped_into_a_name_the_rules_exclude_keeps_its_remote_copy() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("a"), b"A");
    a.create(&mut remote, &site.root.join("b"), b"BB");
    let old_a = a
        .folder
        .engine
        .trees
        .remote()
        .iter()
        .find(|(_, n)| n.shape.name == "a")
        .unwrap()
        .0
        .clone();
    a.create(&mut remote, &site.root.join(".koiignore"), b"b\n");
    fs::rename(site.root.join("a"), site.root.join("tmp")).unwrap();
    fs::rename(site.root.join("b"), site.root.join("a")).unwrap();
    fs::rename(site.root.join("tmp"), site.root.join("b")).unwrap();
    let included_ino = u128::from(fs::metadata(site.root.join("a")).unwrap().ino());
    a.scan_until(|a| {
        a.observer
            .snapshot()
            .entries
            .get(Path::new("a"))
            .is_some_and(|e| e.id.ino == included_ino)
    });
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(fs::read(site.root.join("b")).unwrap(), b"A");
    assert!(
        a.folder
            .engine
            .trees
            .remote()
            .get(&old_a)
            .is_some_and(|n| !n.trashed),
        "the copy moved into an ignored name was deleted from the server"
    );
}

/// Device b holds an untracked symlink at e. The server moves d to e and creates another file at d.
/// The move fails because e is taken, but d is parked out of the way, so the new file arrives at d
/// and the symlink stays.
#[test]
fn a_move_onto_a_name_a_symlink_holds_still_lets_the_new_file_take_the_old_name() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("d"), b"original");
    let (site, mut b) = linked(&mut remote, "b");
    std::os::unix::fs::symlink("untouched", site.root.join("e")).unwrap();
    fs::rename(site_a.root.join("d"), site_a.root.join("e")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    a.create(&mut remote, &site_a.root.join("d"), b"new remote file");
    b.pull(&remote);
    eventually("the new remote file lands at d", || {
        if let Some(work) = b.work.pop_front() {
            b.perform(work, &mut remote).unwrap();
        }
        b.scan(Duration::from_millis(100));
        fs::read(site.root.join("d")).ok().as_deref() == Some(&b"new remote file"[..])
    });
    assert_eq!(
        fs::read_link(site.root.join("e")).unwrap(),
        PathBuf::from("untouched")
    );
    assert_eq!(fs::read(site.root.join("d")).unwrap(), b"new remote file");
}

/// GUN-330. The effect was planned while the name `a` was free in L. Recording its outcome beside the folder L
/// then held at `a` left two nodes with one name, and the store did not load again. The server renames the folder's
/// move to `a`, and the user's rename back to `f` is kept on both devices (GUN-344).
#[test]
fn an_effect_whose_name_l_gave_to_another_node_writes_nothing_and_is_planned_again() {
    let mut remote = Remote::new();
    let ((site_a, mut a), (site, mut b), pending) = planned_beside_a_folder(&mut remote);
    rename_to_a_and_back(&site, &mut b);

    let staged = ready(pending.stage(&remote.chunks));
    let step = b.folder.staged(pending, staged);
    b.take(step).unwrap();
    site.stored();
    assert_eq!(names(&site.root), ["f"], "nothing is written at the name");
    let effects = &b.folder.engine.effects;
    assert!(effects.in_flight().is_empty() && effects.blocked().is_empty());
    let marker = koidrive_fs::read_marker(&site.root).unwrap().unwrap();
    assert_eq!(marker.effects, effects.cleared());
    let staging = site.root.join(koidrive_fs::PRIVATE_DIR).join("staging");
    assert_eq!(fs::read_dir(staging).unwrap().count(), 0);

    sync(&mut remote, (&site_a, &mut a), (&site, &mut b));
    let held = entries(&site.root);
    let texts: Vec<&str> = held
        .iter()
        .filter_map(|(_, text)| text.as_deref())
        .collect();
    assert_eq!(texts.len(), 2, "{held:?}");
    assert!(texts.contains(&"written on a") && texts.contains(&"kept on b"));
    assert_eq!(
        names(&site.root),
        ["a", "f"],
        "the rename back to f is kept"
    );
    assert_eq!(names(&site_a.root), ["a", "f"]);
}

/// The same L, with the file already at its name when the process stops. `Disk::apply` no longer gets there, so
/// the test leaves what a process without the check left: the marker written and the staged file renamed to `a`,
/// with no acknowledgement. The next start finds the effect's file at its name and does not record it. The
/// observer then reports the file as the user's own, so it is uploaded, and both devices end with the bytes twice:
/// at `a` and at the name the server gave the copy.
#[test]
fn a_file_placed_at_a_name_l_gave_away_is_not_recorded_by_the_next_start() {
    let mut remote = Remote::new();
    let ((site_a, mut a), (site, mut b), pending) = planned_beside_a_folder(&mut remote);
    rename_to_a_and_back(&site, &mut b);

    let Ok(staged) = ready(pending.stage(&remote.chunks)) else {
        panic!("the download is staged");
    };
    let record = b.folder.engine.effects.in_flight().first().unwrap().clone();
    let marker = Marker {
        space_id: site.link.space_id.clone(),
        effects: record.counter,
    };
    write_marker(&site.root, &marker).unwrap();
    fs::rename(site.root.join(record.staging()), site.root.join("a")).unwrap();
    drop((staged, pending, b));

    let mut b = site.start();
    let stored = site.stored();
    assert!(stored.effects.in_flight().is_empty() && stored.effects.blocked().is_empty());
    assert_eq!(stored.effects.cleared(), record.counter);
    assert!(stored.trees.local().get(&record.node).is_none());
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    sync(&mut remote, (&site_a, &mut a), (&site, &mut b));
    let held = entries(&site.root);
    let mut texts: Vec<&str> = held
        .iter()
        .filter_map(|(_, text)| text.as_deref())
        .collect();
    texts.sort();
    assert_eq!(
        texts,
        ["kept on b", "written on a", "written on a"],
        "{held:?}"
    );
}

/// The second way to the same store: the folder `a` is planned, the user makes a folder `a` of their own, which a
/// batch reports, and the process stops. The next start finds a folder at the effect's name and takes the effect
/// for done.
#[test]
fn a_restart_that_finds_the_users_folder_at_a_planned_folders_name_keeps_l_a_tree() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    let (site, mut b) = linked(&mut remote, "b");
    fs::create_dir(site_a.root.join("a")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert_eq!(a.run(&mut remote), ["send"]);

    b.pull(&remote);
    let Some(Work::Stage(pending)) = b.work.pop_front() else {
        panic!("the effect that creates a is staged");
    };
    site.stored();
    fs::create_dir(site.root.join("a")).unwrap();
    fs::write(site.root.join("a/kept.txt"), b"kept on b").unwrap();
    b.scan_until(|b| b.folder.engine.trees.local().len() == 2);
    site.stored();
    drop((pending, b));

    let mut b = site.start();
    let stored = site.stored();
    assert!(stored.effects.in_flight().is_empty() && stored.effects.blocked().is_empty());
    assert_eq!(stored.trees.local().len(), 2);
    drop(b);
    b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    sync(&mut remote, (&site_a, &mut a), (&site, &mut b));
    let held = entries(&site.root);
    let texts: Vec<&str> = held
        .iter()
        .filter_map(|(_, text)| text.as_deref())
        .collect();
    assert_eq!(texts, ["kept on b"], "{held:?}");
}

#[test]
fn a_file_moved_over_another_out_of_a_removed_folder_leaves_the_server_across_a_restart_during_a_bounded_search()
 {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let settled = |a: &mut Device, remote: &mut Remote| {
        for _ in 0..3 {
            a.settle();
            a.run(remote);
        }
    };
    a.create(&mut remote, &site.root.join(".koiignore"), b"ignored/\n");
    fs::create_dir(site.root.join("d")).unwrap();
    a.create(&mut remote, &site.root.join("d/f"), b"kept");
    a.create(&mut remote, &site.root.join("x"), b"other");
    fs::create_dir_all(site.root.join("ignored/zzz")).unwrap();
    for n in 0..240 {
        let dir = site.root.join(format!("ignored/d{n:03}"));
        fs::create_dir(&dir).unwrap();
        for m in 0..100 {
            fs::write(dir.join(format!("{m}")), b"x").unwrap();
        }
    }
    fs::hard_link(site.root.join("d/f"), site.root.join("ignored/zzz/l")).unwrap();
    settled(&mut a, &mut remote);

    fs::rename(site.root.join("d/f"), site.root.join("x")).unwrap();
    fs::remove_dir(site.root.join("d")).unwrap();
    a.scan(Duration::from_millis(300));
    a.run(&mut remote);
    drop(a);
    let mut a = site.start();
    a.connect(&remote);
    a.scan_until(|a| a.folder.scanned);
    settled(&mut a, &mut remote);
    settled(&mut a, &mut remote);
    let after = server_nodes(&remote);
    assert!(
        !(after.iter()).any(|(name, _, live)| (name == "d" || name == "f") && *live),
        "{after:?}"
    );
    assert_eq!(a.folder.engine.trees.check(), Ok(()));
}

#[test]
fn a_bulk_delete_with_a_file_moved_over_another_keeps_the_file_at_its_path() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let settled = |a: &mut Device, remote: &mut Remote| {
        for _ in 0..3 {
            a.settle();
            a.run(remote);
        }
    };
    a.create(&mut remote, &site.root.join(".koiignore"), b"ignored/\n");
    for s in 0..4 {
        fs::create_dir_all(site.root.join(format!("d/s{s}"))).unwrap();
        for n in 0..10 {
            a.create(&mut remote, &site.root.join(format!("d/s{s}/g{n}")), b"g");
        }
    }
    a.create(&mut remote, &site.root.join("d/s2/f"), b"kept");
    a.create(&mut remote, &site.root.join("x"), b"other");
    fs::create_dir_all(site.root.join("ignored/zzz")).unwrap();
    for n in 0..240 {
        let dir = site.root.join(format!("ignored/d{n:03}"));
        fs::create_dir(&dir).unwrap();
        for m in 0..100 {
            fs::write(dir.join(format!("{m}")), b"x").unwrap();
        }
    }
    fs::hard_link(site.root.join("d/s2/f"), site.root.join("ignored/zzz/l")).unwrap();
    settled(&mut a, &mut remote);

    fs::rename(site.root.join("d/s2/f"), site.root.join("x")).unwrap();
    fs::remove_dir_all(site.root.join("d")).unwrap();
    for _ in 0..4 {
        settled(&mut a, &mut remote);
    }
    answer(&site, Answer::Delete);
    let step = a.folder.requested();
    a.take(step).unwrap();
    a.run(&mut remote);
    for _ in 0..4 {
        settled(&mut a, &mut remote);
    }
    assert_eq!(
        fs::read(site.root.join("x")).ok(),
        Some(b"kept".to_vec()),
        "{:?}",
        server_nodes(&remote)
    );
}

#[test]
fn a_pending_name_finished_on_a_server_restored_to_before_the_rename_is_finished_again_after_a_restart()
 {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("a"), b"the file");
    let (site_b, mut b) = linked(&mut remote, "b");
    b.settle();
    fs::rename(site_b.root.join("a"), site_b.root.join("x")).unwrap();
    fs::create_dir(site_b.root.join("a")).unwrap();
    fs::rename(site_b.root.join("x"), site_b.root.join("a/f")).unwrap();
    b.rescan();
    b.scan_until(|b| !b.work.is_empty());
    let Some(Work::Send(commit)) = b.work.pop_front() else {
        panic!("the move goes out as a commit");
    };
    let result = remote.commit(&b.caller, &commit);
    let step = b.folder.sent(result);
    b.take(step).unwrap();
    assert_eq!(remote.names(), ["a (pending)", "f"]);
    let mut before_the_rename = None;
    'drive: for _ in 0..200 {
        for device in [&mut a, &mut b] {
            let Some(work) = device.work.pop_front() else {
                device.pull(&remote);
                device.scan(Duration::from_millis(150));
                continue;
            };
            let renames = matches!(&work, Work::Send(commit) if commit.ops.iter().any(|op| {
                matches!(&op.op, Some(op::Op::Move(moved)) if moved.new_name == "a")
            }));
            if renames {
                before_the_rename = Some(remote.space.clone());
            }
            device.perform(work, &mut remote).unwrap();
            if renames {
                break 'drive;
            }
        }
    }
    assert_eq!(remote.names(), ["a", "f"]);
    remote.space = before_the_rename.expect("a device renames the folder to its name");
    drop((a, b));
    let (mut a, mut b) = (site_a.start(), site_b.start());
    for device in [&mut a, &mut b] {
        device.connect(&remote);
        device.scan_until(|device| device.folder.scanned);
    }
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));
    assert_eq!(remote.names(), ["a", "f"]);
    assert_eq!(
        entries(&site_b.root),
        [
            ("a".to_owned(), None),
            ("a/f".to_owned(), Some("the file".to_owned()))
        ]
    );
}
