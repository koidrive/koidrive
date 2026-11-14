// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_server_behind_the_device_gets_back_the_files_it_lost() {
    let mut remote = Remote::new();
    let (site, mut b) = behind_the_device(&mut remote);
    b.pull(&remote);
    assert_eq!(b.state(), FolderState::Bootstrapping);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert_eq!(
        files(&site.root),
        [("one.txt".to_owned(), "one".to_owned())]
    );
    assert_eq!(trashed(&site.root), Vec::<String>::new());
    assert_eq!(remote.names(), ["one.txt"]);
    assert_eq!(b.state(), FolderState::UpToDate);
}

#[test]
fn a_commit_the_server_is_behind_for_reads_the_tree_again_and_sends_what_it_lost() {
    let mut remote = Remote::new();
    let (site, mut b) = behind_the_device(&mut remote);
    fs::write(site.root.join("two.txt"), b"two").unwrap();
    b.scan_until(|b| !b.work.is_empty());
    let ran = b.run(&mut remote);
    assert!(ran.starts_with(&["upload", "send", "list"]), "{ran:?}");
    b.settle();
    b.run(&mut remote);
    assert_eq!(
        files(&site.root),
        [
            ("one.txt".to_owned(), "one".to_owned()),
            ("two.txt".to_owned(), "two".to_owned())
        ]
    );
    assert_eq!(trashed(&site.root), Vec::<String>::new());
    assert_eq!(remote.names(), ["one.txt", "two.txt"]);
    assert_eq!(b.state(), FolderState::UpToDate);
}

#[test]
fn a_folder_moved_into_a_folder_the_server_lost_is_moved_there_again() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("photos")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    let restored = backup(&remote, &a);
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    fs::create_dir(site_a.root.join("albums")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    fs::rename(
        site_a.root.join("photos"),
        site_a.root.join("albums/photos"),
    )
    .unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert!(site.root.join("albums/photos").is_dir());
    remote = restored;
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert_eq!(names(&site.root), ["albums", "photos"]);
    assert_eq!(remote.names(), ["albums", "photos"]);
    a.connect(&remote);
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert!(site.root.join("albums/photos").is_dir());
    assert_eq!(names(&site.root), ["albums"]);
    assert_eq!(trashed(&site.root), Vec::<String>::new());
    let nodes = remote.space.nodes();
    let albums = nodes.iter().find(|node| node.name == "albums").unwrap();
    let photos = nodes.iter().find(|node| node.name == "photos").unwrap();
    assert_eq!(photos.parent_id, albums.node_id);
    assert_eq!(remote.names(), ["albums", "photos"]);
    assert_eq!(b.state(), FolderState::UpToDate);
}

#[test]
fn an_edit_the_server_lost_is_sent_again_by_a_device_that_downloaded_it() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("one.txt"), b"v1");
    let restored = backup(&remote, &a);
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    fs::write(site_a.root.join("one.txt"), b"v2").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    assert_eq!(files(&site.root), [("one.txt".to_owned(), "v2".to_owned())]);
    remote = restored;
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert_eq!(files(&site.root), [("one.txt".to_owned(), "v2".to_owned())]);
    assert_eq!(remote.names(), ["one.txt"]);
    assert_eq!(b.state(), FolderState::UpToDate);
}

#[test]
fn an_edit_the_server_lost_leaves_no_copy_when_both_devices_send_it_again() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("one.txt"), b"v1");
    let restored = backup(&remote, &a);
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    a.create(&mut remote, &site_a.root.join("one.txt"), b"v2");
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    remote = restored;
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    a.event(Event::Connected(remote.space.welcome(&a.caller)))
        .unwrap();
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    let both = [("one.txt".to_owned(), "v2".to_owned())];
    assert_eq!(remote.names(), ["one.txt"]);
    assert_eq!(files(&site.root), both);
    assert_eq!(files(&site_a.root), both);
    assert_eq!(
        (a.state(), b.state()),
        (FolderState::UpToDate, FolderState::UpToDate)
    );
}

/// The server lost the commits that created `albums` and moved `one.txt` into it. While `b` replays the log
/// after its listing, the log brings device `c`'s delete of `one.txt`. `b` deletes the file instead of
/// uploading it again, and creates `albums` again.
#[test]
fn a_delete_replayed_below_a_folder_the_server_lost_still_deletes_the_file() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("one.txt"), b"one");
    let restored = backup(&remote, &a);
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    fs::create_dir(site_a.root.join("albums")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    fs::rename(
        site_a.root.join("one.txt"),
        site_a.root.join("albums/one.txt"),
    )
    .unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert_eq!(files(&site.root), [("albums/one.txt".into(), "one".into())]);
    let trees = &b.folder.engine.trees;
    let one = (trees.local().iter())
        .find(|(_, node)| node.shape.name == "one.txt")
        .unwrap()
        .0
        .clone();
    remote = restored;
    b.pull(&remote);
    let list = b.work.pop_front().unwrap();
    assert!(matches!(list, Work::List { .. }));
    b.perform(list, &mut remote).unwrap();
    assert!(matches!(b.folder.stream, Stream::Replaying(_)));
    let (site_c, mut c) = linked(&mut remote, "c");
    c.settle();
    fs::remove_file(site_c.root.join("one.txt")).unwrap();
    c.scan_until(|c| !c.work.is_empty());
    c.run(&mut remote);
    assert_eq!(remote.names(), Vec::<String>::new());
    let subscribe = b.work.pop_front().unwrap();
    assert!(matches!(subscribe, Work::Subscribe { .. }));
    b.perform(subscribe, &mut remote).unwrap();
    assert!(b.folder.engine.trees.remote().get(&one).unwrap().trashed);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert_eq!(files(&site.root), Vec::<(String, String)>::new());
    assert!(site.root.join("albums").is_dir());
    assert_eq!(remote.names(), ["albums"]);
    assert_eq!(b.state(), FolderState::UpToDate);
}

#[test]
fn a_server_behind_the_device_whose_head_catches_up_before_the_listing_gets_back_the_files_it_lost()
{
    let mut remote = Remote::new();
    let (site, mut b) = behind_the_device(&mut remote);
    b.pull(&remote);
    assert_eq!(b.state(), FolderState::Bootstrapping);
    catch_up(&mut remote, &b);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    caught_up(&site, &b, &remote, &[]);
}

#[test]
fn a_commit_the_server_is_behind_for_sends_what_it_lost_when_the_head_catches_up_before_the_listing()
 {
    let mut remote = Remote::new();
    let (site, mut b) = behind_the_device(&mut remote);
    fs::write(site.root.join("two.txt"), b"two").unwrap();
    b.scan_until(|b| !b.work.is_empty());
    while !matches!(b.folder.stream, Stream::Listing { .. }) {
        let work = b.work.pop_front().expect("the commit is refused");
        b.perform(work, &mut remote).unwrap();
    }
    catch_up(&mut remote, &b);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    caught_up(&site, &b, &remote, &["two"]);
}

#[test]
fn a_device_that_restarts_after_the_server_behind_it_caught_up_still_gets_back_the_files_it_lost() {
    let mut remote = Remote::new();
    let (site, mut b) = behind_the_device(&mut remote);
    b.pull(&remote);
    catch_up(&mut remote, &b);
    drop(b);
    let mut b = site.start();
    b.connect(&remote);
    assert_eq!(b.run(&mut remote), ["list", "subscribe"]);
    b.settle();
    b.run(&mut remote);
    caught_up(&site, &b, &remote, &[]);
}

/// The task stops right after the transaction that takes the refusal, before it lists the tree, and the server's head
/// then catches up. The store still knows that the server is behind.
#[test]
fn a_commit_the_server_is_behind_for_sends_what_it_lost_after_a_crash_right_after_the_refusal() {
    let mut remote = Remote::new();
    let (site, mut b) = behind_the_device(&mut remote);
    fs::write(site.root.join("two.txt"), b"two").unwrap();
    b.scan_until(|b| !b.work.is_empty());
    loop {
        let work = b.work.pop_front().expect("the commit is sent");
        if matches!(work, Work::Send(_)) {
            b.folder.crash_after = Some(0);
            b.perform(work, &mut remote).unwrap_err();
            break;
        }
        b.perform(work, &mut remote).unwrap();
    }
    assert!(b.folder.engine.commits.in_flight().is_none());
    catch_up(&mut remote, &b);
    drop(b);
    let mut b = site.start();
    b.connect(&remote);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    caught_up(&site, &b, &remote, &["two"]);
}

/// The server's storage is restored from a backup taken after \`a\`'s first commit, so it counts fewer commits of
/// \`a\` than \`a\`'s store holds (GUN-379). \`a\` lists the tree again and sends the commit the server lost.
#[test]
fn a_server_that_lost_a_commit_of_this_device_gets_it_back() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("one.txt"), b"one");
    let mut restored = backup(&remote, &a);
    a.create(&mut remote, &site.root.join("two.txt"), b"two");
    let welcome = restored.space.welcome(&a.caller);
    assert_eq!(
        (welcome.device_seq, a.folder.engine.commits.accepted()),
        (1, 2)
    );
    a.event(Event::Connected(welcome)).unwrap();
    a.run(&mut restored);
    a.settle();
    a.run(&mut restored);
    assert_eq!(restored.names(), ["one.txt", "two.txt"]);
    assert_eq!(names(&site.root), ["one.txt", "two.txt"]);
    assert_eq!(trashed(&site.root), Vec::<String>::new());
    assert_eq!(a.state(), FolderState::UpToDate);
}

/// As above, but \`b\` commits to the restored server twice before \`a\` reconnects, so the server's head is past
/// \`a\`'s cursor and nothing in the \`Welcome\` but the count shows what was lost. \`a\` still lists the tree again
/// instead of following the log from its cursor, and both devices' files are kept.
#[test]
fn a_server_that_lost_a_commit_of_this_device_gets_it_back_after_others_committed_past_the_cursor()
{
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("one.txt"), b"one");
    let mut restored = backup(&remote, &a);
    a.create(&mut remote, &site.root.join("two.txt"), b"two");
    let (other, mut b) = linked(&mut restored, "b");
    b.create(&mut restored, &other.root.join("three.txt"), b"three");
    b.create(&mut restored, &other.root.join("four.txt"), b"four");
    let welcome = restored.space.welcome(&a.caller);
    let cursor = a.folder.engine.cursor.as_ref().unwrap().last_seq;
    assert!(welcome.head_seq > cursor, "{} > {cursor}", welcome.head_seq);
    a.event(Event::Connected(welcome)).unwrap();
    a.run(&mut restored);
    a.settle();
    a.run(&mut restored);
    let all = ["four.txt", "one.txt", "three.txt", "two.txt"];
    assert_eq!(restored.names(), all);
    assert_eq!(names(&site.root), all);
    assert_eq!(trashed(&site.root), Vec::<String>::new());
    assert_eq!(a.state(), FolderState::UpToDate);
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Answered {
    Received,
    Dropped,
    ResendRefused(Code),
}

fn created_in_a_folder_then_listed_and_lost(answered: Answered) -> (Vec<(String, String)>, Vec<String>) {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::create_dir(site.root.join("docs")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    let mut restored = backup(&remote, &a);
    a.create(&mut remote, &site.root.join("one.txt"), b"one");
    fs::write(site.root.join("docs/n.txt"), b"only on a").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let upload = a.work.pop_front().unwrap();
    a.perform(upload, &mut remote).unwrap();
    let Some(Work::Send(commit)) = a.work.pop_front() else {
        panic!("the commit is sent after its upload");
    };
    let result = remote.commit(&a.caller, &commit);
    if answered == Answered::Received {
        let step = a.folder.sent(result);
        a.take(step).unwrap();
    }
    a.event(Event::ResyncRequired(ResyncRequired {
        reason: Reason::LogTruncated as i32,
        ..ResyncRequired::default()
    }))
    .unwrap();
    a.run(&mut remote);
    if let Answered::ResendRefused(code) = answered {
        let welcome = remote.space.welcome(&a.caller);
        assert_eq!(welcome.device_seq, a.folder.engine.commits.accepted() + 1);
        a.event(Event::Connected(welcome)).unwrap();
        a.run(&mut remote);
        let refusal = ProtoError {
            code: code as i32,
            ..Default::default()
        };
        let step = a.folder.sent(Err(CallError::Server(ServerError(refusal))));
        a.take(step).unwrap();
        a.work.retain(|work| !matches!(work, Work::ResendAfter(_)));
        a.run(&mut remote);
    }
    let (other, mut b) = linked(&mut restored, "b");
    b.settle();
    fs::remove_dir(other.root.join("docs")).unwrap();
    b.scan_until(|b| !b.work.is_empty());
    b.run(&mut restored);
    let welcome = restored.space.welcome(&a.caller);
    assert!(welcome.device_seq < a.folder.engine.commits.accepted());
    a.event(Event::Connected(welcome)).unwrap();
    a.run(&mut restored);
    a.settle();
    a.run(&mut restored);
    (files(&site.root), restored.names())
}

#[test]
fn a_create_whose_result_never_came_is_sent_again_after_a_listing_whose_pages_held_it() {
    let received = created_in_a_folder_then_listed_and_lost(Answered::Received);
    assert!(received.0.contains(&("docs/n.txt".into(), "only on a".into())), "{received:?}");
    for answered in [
        Answered::Dropped,
        Answered::ResendRefused(Code::RetryLater),
        Answered::ResendRefused(Code::Unauthenticated),
    ] {
        let lost = created_in_a_folder_then_listed_and_lost(answered);
        assert_eq!(lost, received, "{answered:?} changed the outcome");
    }
}

#[test]
fn a_listing_that_waits_for_a_sent_commit_replaces_r_once_the_commit_is_abandoned() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::write(site.root.join("n.txt"), b"first").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let upload = a.work.pop_front().unwrap();
    a.perform(upload, &mut remote).unwrap();
    let Some(Work::Send(_)) = a.work.pop_front() else {
        panic!("the commit is sent after its upload");
    };
    a.event(Event::ResyncRequired(ResyncRequired {
        reason: Reason::LogTruncated as i32,
        ..ResyncRequired::default()
    }))
    .unwrap();
    a.run(&mut remote);
    assert!(matches!(a.folder.stream, Stream::Replaying(_)));
    let missing = ProtoError {
        code: Code::MissingChunks as i32,
        ..Default::default()
    };
    let step = a.folder.sent(Err(CallError::Server(ServerError(missing))));
    a.take(step).unwrap();
    let Some(Work::Upload(_)) = a.work.pop_front() else {
        panic!("the missing content is uploaded again");
    };
    fs::write(site.root.join("n.txt"), b"second").unwrap();
    let step = a.folder.uploaded(Err(ChunkError::SourceChanged { hash: vec![0; 32] }));
    a.take(step).unwrap();
    assert!(a.folder.engine.commits.in_flight().is_none());
    a.settle();
    a.rest(&mut remote);
    assert_eq!(remote.names(), ["n.txt"]);
    assert_eq!(a.state(), FolderState::UpToDate);
}

/// The server's storage is restored from a backup taken before `a` deleted `one.txt` (GUN-359). `a` lists the tree
/// again, which holds `one.txt` live as it was deleted, and sends the delete again instead of downloading the file.
#[test]
fn a_delete_the_server_lost_is_sent_again() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("one.txt"), b"one");
    a.create(&mut remote, &site.root.join("two.txt"), b"two");
    let mut restored = backup(&remote, &a);
    fs::remove_file(site.root.join("one.txt")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert_eq!(a.run(&mut remote), ["send"]);
    assert_eq!(remote.names(), ["two.txt"]);
    a.event(Event::Connected(restored.space.welcome(&a.caller)))
        .unwrap();
    a.run(&mut restored);
    a.settle();
    a.run(&mut restored);
    assert_eq!(restored.names(), ["two.txt"]);
    assert_eq!(names(&site.root), ["two.txt"]);
    assert_eq!(a.state(), FolderState::UpToDate);
}

/// As above, but `b` writes new content into `one.txt` on the restored server before `a` reconnects, at the seq
/// `a`'s lost delete had. `a` does not send the delete again: it downloads `b`'s content.
#[test]
fn a_delete_the_server_lost_is_not_sent_again_over_another_devices_edit() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("one.txt"), b"one");
    let mut restored = backup(&remote, &a);
    fs::remove_file(site.root.join("one.txt")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert_eq!(a.run(&mut remote), ["send"]);
    let (other, mut b) = linked(&mut restored, "b");
    b.create(&mut restored, &other.root.join("one.txt"), b"edited");
    a.event(Event::Connected(restored.space.welcome(&a.caller)))
        .unwrap();
    a.run(&mut restored);
    a.settle();
    a.run(&mut restored);
    assert_eq!(restored.names(), ["one.txt"]);
    assert_eq!(fs::read(site.root.join("one.txt")).unwrap(), b"edited");
    assert_eq!(a.state(), FolderState::UpToDate);
}

/// The server's storage is restored from a backup taken after `a` created `one.txt`, before three more commits of
/// `a` and its delete of `one.txt` at seq 5. On the restored server `b` deletes `one.txt` at seq 2 and a person
/// restores it at seq 3. The restore brings back the shape `a` deleted, at a seq below the lost delete's, so only the
/// seq of `a`'s last commit the server still holds shows it came after the loss. `a` keeps the restore.
#[test]
fn a_restore_after_the_server_lost_a_delete_is_kept() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("one.txt"), b"one");
    let mut restored = backup(&remote, &a);
    for pad in ["pad-two.txt", "pad-three.txt", "pad-four.txt"] {
        a.create(&mut remote, &site.root.join(pad), pad.as_bytes());
    }
    fs::remove_file(site.root.join("one.txt")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert_eq!(a.run(&mut remote), ["send"]);
    assert_eq!(remote.space.head_seq(), 5);
    let (other, mut b) = linked(&mut restored, "b");
    let node = (b.folder.engine.trees.local().iter())
        .find(|(_, node)| node.shape.name == "one.txt")
        .unwrap()
        .0
        .clone();
    fs::remove_file(other.root.join("one.txt")).unwrap();
    b.scan_until(|b| !b.work.is_empty());
    assert_eq!(b.run(&mut restored), ["send"]);
    let intent = RestoreIntent::from_trash(node.clone(), None, None).unwrap();
    let request = ask(&other, &intent);
    let step = b.folder.requested();
    b.take(step).unwrap();
    b.run(&mut restored);
    assert_eq!(ended(&other), [(request, Outcome::Restored)]);
    assert_eq!(
        b.folder.engine.trees.remote().get(&node).unwrap().meta_rev,
        3
    );
    a.event(Event::Connected(restored.space.welcome(&a.caller)))
        .unwrap();
    a.run(&mut restored);
    a.settle();
    a.run(&mut restored);
    let all = ["one.txt", "pad-four.txt", "pad-three.txt", "pad-two.txt"];
    assert_eq!(restored.names(), all);
    assert_eq!(names(&site.root), all);
    assert_eq!(fs::read(site.root.join("one.txt")).unwrap(), b"one");
    assert_eq!(a.state(), FolderState::UpToDate);
}

/// As above, but the server refused a later commit of `a` with `FORBIDDEN`, so `a` is under a policy halt when it
/// meets the restored server. The halted `a` lists the tree after a restart, and once the halt is lifted it still
/// keeps the restore instead of sending its delete again.
#[test]
fn a_restore_after_the_server_lost_a_delete_under_a_policy_halt_is_kept() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("one.txt"), b"one");
    let mut restored = backup(&remote, &a);
    for pad in ["pad-two.txt", "pad-three.txt", "pad-four.txt"] {
        a.create(&mut remote, &site.root.join(pad), pad.as_bytes());
    }
    fs::remove_file(site.root.join("one.txt")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert_eq!(a.run(&mut remote), ["send"]);
    assert_eq!(remote.space.head_seq(), 5);
    fs::write(site.root.join("refused.txt"), b"refused").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let upload = a.work.pop_front().unwrap();
    a.perform(upload, &mut remote).unwrap();
    assert!(matches!(a.work.pop_front(), Some(Work::Send(_))));
    let step = a.folder.sent(Err(CallError::Server(ServerError(
        koidrive_proto::v1::Error {
            code: Code::Forbidden as i32,
            message: "refused".into(),
            ..Default::default()
        },
    ))));
    assert!(a.take(step).is_err());
    let (other, mut b) = linked(&mut restored, "b");
    let node = (b.folder.engine.trees.local().iter())
        .find(|(_, node)| node.shape.name == "one.txt")
        .unwrap()
        .0
        .clone();
    fs::remove_file(other.root.join("one.txt")).unwrap();
    b.scan_until(|b| !b.work.is_empty());
    assert_eq!(b.run(&mut restored), ["send"]);
    let intent = RestoreIntent::from_trash(node, None, None).unwrap();
    let request = ask(&other, &intent);
    let step = b.folder.requested();
    b.take(step).unwrap();
    b.run(&mut restored);
    assert_eq!(ended(&other), [(request, Outcome::Restored)]);
    drop(a);
    let mut halted = restarted(&site, &mut restored);
    halted.settle();
    halted.run(&mut restored);
    assert!(halted.sent.is_empty());
    drop(halted);
    lift_halt(&site.db, Lift::Replan).unwrap();
    let mut a = restarted(&site, &mut restored);
    a.settle();
    a.run(&mut restored);
    let all = [
        "one.txt",
        "pad-four.txt",
        "pad-three.txt",
        "pad-two.txt",
        "refused.txt",
    ];
    assert_eq!(restored.names(), all);
    assert_eq!(names(&site.root), all);
    assert_eq!(fs::read(site.root.join("one.txt")).unwrap(), b"one");
    assert_eq!(a.state(), FolderState::UpToDate);
}

/// The server's storage is restored from a backup taken before `a` wrote new content into `one.txt` (GUN-387).
/// `a` lists the tree again, which holds `one.txt` with its older content, and sends its edit again instead of
/// keeping it beside `one.txt` as a conflict copy.
#[test]
fn an_edit_the_server_lost_is_sent_again() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("one.txt"), b"one");
    let mut restored = backup(&remote, &a);
    a.create(&mut remote, &site.root.join("one.txt"), b"edited");
    a.event(Event::Connected(restored.space.welcome(&a.caller)))
        .unwrap();
    a.run(&mut restored);
    a.settle();
    a.run(&mut restored);
    assert_eq!(restored.names(), ["one.txt"]);
    assert_eq!(names(&site.root), ["one.txt"]);
    assert_eq!(fs::read(site.root.join("one.txt")).unwrap(), b"edited");
    assert_eq!(a.state(), FolderState::UpToDate);
    let (other, _b) = linked(&mut restored, "b");
    assert_eq!(fs::read(other.root.join("one.txt")).unwrap(), b"edited");
}

/// As above, but device `b`, which never saw `a`'s edit, deletes `one.txt` on the restored server first. An edit
/// beats a delete (R8): `a` sends its edit again, the server revives `one.txt`, and `a` keeps it in the folder.
#[test]
fn an_edit_the_server_lost_revives_a_file_another_device_deleted() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("one.txt"), b"one");
    let mut restored = backup(&remote, &a);
    a.create(&mut remote, &site.root.join("one.txt"), b"edited");
    let (other, mut b) = linked(&mut restored, "b");
    fs::remove_file(other.root.join("one.txt")).unwrap();
    b.scan_until(|b| !b.work.is_empty());
    assert_eq!(b.run(&mut restored), ["send"]);
    assert!(restored.names().is_empty());
    a.event(Event::Connected(restored.space.welcome(&a.caller)))
        .unwrap();
    a.run(&mut restored);
    a.settle();
    a.run(&mut restored);
    assert_eq!(restored.names(), ["one.txt"]);
    assert_eq!(names(&site.root), ["one.txt"]);
    assert_eq!(fs::read(site.root.join("one.txt")).unwrap(), b"edited");
    assert!(trashed(&site.root).is_empty());
    assert_eq!(a.state(), FolderState::UpToDate);
    let (fresh, _c) = linked(&mut restored, "c");
    assert_eq!(fs::read(fresh.root.join("one.txt")).unwrap(), b"edited");
}

/// `a` edited `one.txt` in a commit the server lost, and the server refused a later commit of `a` with
/// `FORBIDDEN`, so `a` is under a policy halt when it meets the restored server. `b` edits `one.txt` there at seq
/// 2, a seq `a`'s lost commits had. The halted `a` lists the tree after a restart, and once the halt is lifted it
/// keeps `b`'s content in `one.txt` and its own as a conflict copy, instead of sending its edit again over `b`'s.
#[test]
fn an_edit_the_server_lost_under_a_policy_halt_is_not_sent_again_over_a_later_edit() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("one.txt"), b"original");
    let mut restored = backup(&remote, &a);
    for pad in ["pad-two.txt", "pad-three.txt", "pad-four.txt"] {
        a.create(&mut remote, &site.root.join(pad), pad.as_bytes());
    }
    a.create(&mut remote, &site.root.join("one.txt"), b"authored");
    assert_eq!(remote.space.head_seq(), 5);
    fs::write(site.root.join("refused.txt"), b"refused").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let upload = a.work.pop_front().unwrap();
    a.perform(upload, &mut remote).unwrap();
    assert!(matches!(a.work.pop_front(), Some(Work::Send(_))));
    let step = a.folder.sent(Err(CallError::Server(ServerError(
        koidrive_proto::v1::Error {
            code: Code::Forbidden as i32,
            message: "refused".into(),
            ..Default::default()
        },
    ))));
    assert!(a.take(step).is_err());
    let halt = Some(CommitHalt::Policy {
        code: Code::Forbidden as i32,
    });
    assert_eq!(site.stored().commits.halted(), halt);
    let (other, mut b) = linked(&mut restored, "b");
    b.create(&mut restored, &other.root.join("one.txt"), b"post-loss");
    assert_eq!(restored.space.head_seq(), 2);
    drop(a);
    let mut halted = restarted(&site, &mut restored);
    halted.settle();
    halted.run(&mut restored);
    assert!(halted.sent.is_empty());
    assert_eq!(site.stored().commits.halted(), halt);
    drop(halted);
    lift_halt(&site.db, Lift::Replan).unwrap();
    let mut a = restarted(&site, &mut restored);
    a.settle();
    a.run(&mut restored);
    assert_eq!(a.state(), FolderState::UpToDate);
    let (fresh, _c) = linked(&mut restored, "c");
    assert_eq!(fs::read(fresh.root.join("one.txt")).unwrap(), b"post-loss");
    let copy = fresh.root.join("one (conflict from a 2023-11-14).txt");
    assert_eq!(fs::read(copy).unwrap(), b"authored");
}

/// As above, but `b` then commits until the restored server's head passes `a`'s cursor, and edits `one.txt` again
/// at seq 6, so the server sends `a` no `AHEAD_OF_SERVER`. The halted `a` still lists the tree, and once the halt is
/// lifted it keeps `b`'s newest content in `one.txt` and its own as a conflict copy, instead of downloading `b`'s
/// content over its edit.
#[test]
fn an_edit_the_server_lost_under_a_policy_halt_is_kept_when_the_server_passed_the_cursor() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("one.txt"), b"original");
    let mut restored = backup(&remote, &a);
    for pad in ["pad-two.txt", "pad-three.txt", "pad-four.txt"] {
        a.create(&mut remote, &site.root.join(pad), pad.as_bytes());
    }
    a.create(&mut remote, &site.root.join("one.txt"), b"authored");
    assert_eq!(remote.space.head_seq(), 5);
    fs::write(site.root.join("refused.txt"), b"refused").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let upload = a.work.pop_front().unwrap();
    a.perform(upload, &mut remote).unwrap();
    assert!(matches!(a.work.pop_front(), Some(Work::Send(_))));
    let step = a.folder.sent(Err(CallError::Server(ServerError(
        koidrive_proto::v1::Error {
            code: Code::Forbidden as i32,
            message: "refused".into(),
            ..Default::default()
        },
    ))));
    assert!(a.take(step).is_err());
    let (other, mut b) = linked(&mut restored, "b");
    b.create(&mut restored, &other.root.join("one.txt"), b"post-loss");
    for pad in ["b-two.txt", "b-three.txt", "b-four.txt"] {
        b.create(&mut restored, &other.root.join(pad), pad.as_bytes());
    }
    b.create(&mut restored, &other.root.join("one.txt"), b"post-loss-2");
    assert_eq!(restored.space.head_seq(), 6);
    drop(a);
    let mut halted = restarted(&site, &mut restored);
    halted.settle();
    halted.run(&mut restored);
    assert!(halted.sent.is_empty());
    drop(halted);
    lift_halt(&site.db, Lift::Replan).unwrap();
    let mut a = restarted(&site, &mut restored);
    a.settle();
    a.run(&mut restored);
    assert_eq!(a.state(), FolderState::UpToDate);
    let (fresh, _c) = linked(&mut restored, "c");
    assert_eq!(
        fs::read(fresh.root.join("one.txt")).unwrap(),
        b"post-loss-2"
    );
    let copy = fresh.root.join("one (conflict from a 2023-11-14).txt");
    assert_eq!(fs::read(copy).unwrap(), b"authored");
}

/// `a` is under a policy halt when it meets a server that lost its commits, and `b` creates `docs/plan.txt`
/// there. The first halted run lists the tree. `b` then renames `docs` to `papers` before a second halted run, and
/// `papers` to `notes` before the halt is lifted. The loss was handled by the first listing, so neither later run
/// takes `b`'s renames for lost changes and puts an old name back.
#[test]
fn renames_after_a_server_loss_was_handled_under_a_policy_halt_are_kept() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("one.txt"), b"one");
    let mut restored = backup(&remote, &a);
    for pad in ["pad-two.txt", "pad-three.txt"] {
        a.create(&mut remote, &site.root.join(pad), pad.as_bytes());
    }
    fs::write(site.root.join("refused.txt"), b"refused").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let upload = a.work.pop_front().unwrap();
    a.perform(upload, &mut remote).unwrap();
    assert!(matches!(a.work.pop_front(), Some(Work::Send(_))));
    let step = a.folder.sent(Err(CallError::Server(ServerError(
        koidrive_proto::v1::Error {
            code: Code::Forbidden as i32,
            message: "refused".into(),
            ..Default::default()
        },
    ))));
    assert!(a.take(step).is_err());
    let (other, mut b) = linked(&mut restored, "b");
    fs::create_dir(other.root.join("docs")).unwrap();
    b.create(&mut restored, &other.root.join("docs/plan.txt"), b"plan");
    drop(a);
    for (from, to) in [("docs", "papers"), ("papers", "notes")] {
        let mut halted = restarted(&site, &mut restored);
        halted.settle();
        halted.run(&mut restored);
        assert!(halted.sent.is_empty());
        drop(halted);
        fs::rename(other.root.join(from), other.root.join(to)).unwrap();
        b.scan_until(|b| !b.work.is_empty());
        b.run(&mut restored);
    }
    lift_halt(&site.db, Lift::Replan).unwrap();
    let mut a = restarted(&site, &mut restored);
    a.settle();
    a.run(&mut restored);
    a.settle();
    a.run(&mut restored);
    let all = [
        "notes",
        "one.txt",
        "pad-three.txt",
        "pad-two.txt",
        "refused.txt",
    ];
    assert_eq!(names(&site.root), all);
    assert_eq!(fs::read(site.root.join("notes/plan.txt")).unwrap(), b"plan");
    assert_eq!(a.state(), FolderState::UpToDate);
    let (fresh, _c) = linked(&mut restored, "c");
    assert_eq!(names(&fresh.root), all);
}

/// The server lost `a`'s commit of `two.txt`, and `b` deleted `one.txt` on the restored server. The listing reads
/// the trash too, so `a` takes `one.txt` for a remote delete and removes it, instead of creating it again under an
/// ID the server holds in its trash, which it would refuse with `ID_EXISTS`.
#[test]
fn a_file_another_device_deleted_after_the_server_lost_commits_is_removed_here() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("one.txt"), b"one");
    let mut restored = backup(&remote, &a);
    a.create(&mut remote, &site.root.join("two.txt"), b"two");
    let (other, mut b) = linked(&mut restored, "b");
    fs::remove_file(other.root.join("one.txt")).unwrap();
    b.scan_until(|b| !b.work.is_empty());
    assert_eq!(b.run(&mut restored), ["send"]);
    a.event(Event::Connected(restored.space.welcome(&a.caller)))
        .unwrap();
    a.run(&mut restored);
    a.settle();
    a.run(&mut restored);
    assert_eq!(restored.names(), ["two.txt"]);
    assert_eq!(names(&site.root), ["two.txt"]);
    assert_eq!(a.state(), FolderState::UpToDate);
}

/// `a` lists the tree after its log was truncated, and reconnects to a server that lost its commit and took two of
/// `b`'s at the same seqs before the listing returns. The pages read from the old server are dropped and the tree is
/// listed again, so R holds the restored server's files, not the old snapshot with its seq 1.
#[test]
fn a_listing_out_when_the_server_lost_this_devices_commits_is_read_again() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("one.txt"), b"one");
    a.event(Event::ResyncRequired(ResyncRequired {
        reason: Reason::LogTruncated as i32,
        ..ResyncRequired::default()
    }))
    .unwrap();
    let Some(Work::List { .. }) = a.work.pop_front() else {
        panic!("list");
    };
    let old = remote.pages(TREE_PAGE);
    let mut restored = Remote::new();
    let (other, mut b) = linked(&mut restored, "b");
    b.create(&mut restored, &other.root.join("new-one.txt"), b"new one");
    b.create(&mut restored, &other.root.join("new-two.txt"), b"new two");
    a.event(Event::Connected(restored.space.welcome(&a.caller)))
        .unwrap();
    let step = a.folder.listed(Ok(Listed::Done {
        pages: old,
        trash: vec![],
    }));
    a.take(step).unwrap();
    a.run(&mut restored);
    a.settle();
    a.run(&mut restored);
    let all = ["new-one.txt", "new-two.txt", "one.txt"];
    assert_eq!(restored.names(), all);
    assert_eq!(names(&site.root), all);
    assert_eq!(a.state(), FolderState::UpToDate);
}

/// A reconnect's `Welcome` counts one commit of `a`, and the commit in flight is accepted after it. When the send
/// finishes while the `Connected` event is still queued, the folder takes the event first, so the count is not older
/// than what it acknowledged, and the healthy server is not taken for one that lost a commit.
#[test]
fn a_welcome_queued_before_a_finished_send_is_taken_first() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("one.txt"), b"one");
    fs::write(site.root.join("two.txt"), b"two").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let upload = a.work.pop_front().unwrap();
    a.perform(upload, &mut remote).unwrap();
    let Some(Work::Send(commit)) = a.work.pop_front() else {
        panic!("the commit is sent after its upload");
    };
    let queued = remote.space.welcome(&a.caller);
    assert_eq!(queued.device_seq, 1);
    let result = remote.commit(&a.caller, &commit).unwrap();
    let step = sent_in_order(&mut a.folder, [Event::Connected(queued)], Ok(result));
    a.take(step).unwrap();
    assert_eq!(a.folder.engine.commits.accepted(), 2);
    assert!(!a.work.iter().any(|work| matches!(work, Work::List { .. })));
    a.pull(&remote);
    a.run(&mut remote);
    a.settle();
    fs::write(site.root.join("three.txt"), b"three").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.try_run(&mut remote).unwrap();
    assert_eq!(remote.names(), ["one.txt", "three.txt", "two.txt"]);
    assert_eq!(a.folder.engine.commits.accepted(), 3);
}

/// A commit in flight when the task stopped has a slot the restored server cannot account for. It is dropped
/// rather than sent, and what it held is planned again once the tree is listed.
#[test]
fn a_commit_in_flight_when_the_server_lost_this_devices_commits_is_planned_again() {
    let mut remote = Remote::new();
    let (site, commit) = stopped_with_a_commit_in_flight(&mut remote);
    let mut restored = Remote::new();
    let mut a = site.start();
    let welcome = restored.space.welcome(&a.caller);
    assert_eq!((welcome.device_seq, commit.device_seq), (0, 2));
    a.event(Event::Connected(welcome)).unwrap();
    assert!(a.folder.engine.commits.in_flight().is_none());
    assert_eq!(a.folder.engine.commits.accepted(), 0);
    let ran = a.run(&mut restored);
    assert!(!ran.is_empty() && ran[0] == "list", "{ran:?}");
    a.settle();
    a.run(&mut restored);
    assert_eq!(restored.names(), ["draft.txt", "notes.txt"]);
    assert!(a.sent.iter().all(|sent| sent.commit_id != commit.commit_id));
    assert_eq!(a.state(), FolderState::UpToDate);
}

/// The session reports that it did not send the commit in flight again before the folder has taken the \`Welcome\`
/// with the server's lower count. The folder waits for that \`Welcome\` instead of stopping, and then goes on as
/// above.
#[test]
fn a_commit_the_session_did_not_send_again_to_a_server_that_lost_commits_waits_for_the_welcome() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("notes.txt"), b"written on a");
    let mut restored = Remote::new();
    fs::write(site.root.join("draft.txt"), b"in flight").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let upload = a.work.pop_front().unwrap();
    a.perform(upload, &mut remote).unwrap();
    let Some(Work::Send(commit)) = a.work.pop_front() else {
        panic!("the commit is sent after its upload");
    };
    let not_resent = NotResent::DeviceSeqMismatch {
        slot: commit.device_seq,
        welcome: 0,
    };
    let step = a.folder.sent(Err(CallError::NotResent(not_resent)));
    a.take(step).unwrap();
    a.connect(&restored);
    a.run(&mut restored);
    a.settle();
    a.run(&mut restored);
    assert_eq!(restored.names(), ["draft.txt", "notes.txt"]);
    assert_eq!(a.state(), FolderState::UpToDate);
}

#[test]
fn an_edit_whose_commit_is_abandoned_after_the_server_reported_a_loss_is_sent_again() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("notes.txt"), b"written on a");
    fs::write(site.root.join("notes.txt"), b"edited on a").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let upload = a.work.pop_front().unwrap();
    a.perform(upload, &mut remote).unwrap();
    let Some(Work::Send(commit)) = a.work.pop_front() else {
        panic!("the edit is sent after its upload");
    };
    let Some(op::Op::UpdateContent(update)) = &commit.ops[0].op else {
        panic!("the commit edits notes.txt: {:?}", kinds(&commit));
    };
    a.event(Event::ResyncRequired(ResyncRequired {
        reason: Reason::AheadOfServer as i32,
        restore_seq: 0,
        restore_point_ms: 0,
    }))
    .unwrap();
    assert!(a.folder.engine.commits.in_flight().is_some());
    let step = a
        .folder
        .sent(Err(CallError::Server(put_off(Code::HeadMoved))));
    a.take(step).unwrap();
    assert!(a.folder.engine.commits.in_flight().is_none());
    assert!(a.folder.engine.trees.state().dropped_edits.contains(&update.node_id));
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert!(a.folder.engine.trees.state().dropped_edits.is_empty());
    assert_eq!(remote.names(), ["notes.txt"]);
    let (other, _b) = linked(&mut remote, "b");
    assert_eq!(fs::read(other.root.join("notes.txt")).unwrap(), b"edited on a");
}
