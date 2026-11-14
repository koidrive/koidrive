// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_restore_asked_for_brings_a_trashed_file_back_into_the_folder() {
    let mut remote = Remote::new();
    let (site, mut a, node) = with_a_trashed_file(&mut remote);

    let intent = RestoreIntent::from_trash(node.clone(), None, None).unwrap();
    let id = ask(&site, &intent);
    let step = a.folder.requested();
    a.take(step).unwrap();
    assert_eq!(a.run(&mut remote), ["send", "stage"]);
    assert_eq!(kinds(a.sent.last().unwrap()), ["restore"]);
    assert_eq!(remote.names(), ["notes.txt"]);
    assert_eq!(files(&site.root), notes());
    assert_eq!(node_named(&a, "notes.txt"), node);
    assert_eq!(ended(&site), [(id, Outcome::Restored)]);

    a.settle();
    assert!(a.work.is_empty());
    assert_eq!(a.sent.len(), 3);
    assert_eq!(a.state(), FolderState::UpToDate);
}

#[test]
fn a_restore_asked_for_while_the_folder_was_stopped_is_sent_after_the_next_start() {
    let mut remote = Remote::new();
    let (site, a, node) = with_a_trashed_file(&mut remote);
    drop(a);
    let id = ask(&site, &RestoreIntent::from_trash(node, None, None).unwrap());

    let mut a = site.start();
    let queued: Vec<&String> = a.folder.engine.commits.restores().keys().collect();
    assert_eq!(queued, [&id]);
    a.connect(&remote);
    a.scan_until(|a| a.folder.scanned);
    a.run(&mut remote);
    assert_eq!(kinds(a.sent.last().unwrap()), ["restore"]);
    assert_eq!(files(&site.root), notes());
    assert_eq!(ended(&site), [(id, Outcome::Restored)]);
}

#[test]
fn a_restore_is_applied_once_wherever_the_folder_stops() {
    for cut in ["asked", "taken", "applied", "answered"] {
        let mut remote = Remote::new();
        let (site, mut a, node) = with_a_trashed_file(&mut remote);
        let id = ask(&site, &RestoreIntent::from_trash(node, None, None).unwrap());
        if cut != "asked" {
            let step = a.folder.requested();
            a.take(step).unwrap();
            let Some(Work::Send(commit)) = a.work.pop_front() else {
                panic!("a restore alone is sent without an upload");
            };
            if cut != "taken" {
                let result = remote.commit(&a.caller, &commit);
                if cut == "answered" {
                    let step = a.folder.sent(result);
                    a.take(step).unwrap();
                }
            }
        }
        drop(a);

        let mut a = site.start();
        a.connect(&remote);
        a.scan_until(|a| a.folder.scanned);
        a.run(&mut remote);
        a.settle();
        assert!(a.work.is_empty(), "{cut}");
        assert_eq!(files(&site.root), notes(), "{cut}");
        assert_eq!(ended(&site), [(id, Outcome::Restored)], "{cut}");
        assert_eq!(remote.space.head_seq(), 3, "{cut}");
        assert_eq!(a.state(), FolderState::UpToDate, "{cut}");
    }
}

#[test]
fn a_restore_of_an_earlier_version_rewrites_the_file_in_the_folder() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let path = site.root.join("notes.txt");
    a.create(&mut remote, &path, b"the first words");
    fs::write(&path, b"and the second").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert_eq!(a.run(&mut remote), ["upload", "send"]);
    let node = node_named(&a, "notes.txt");
    let request = ListVersions {
        node_id: node.clone(),
        ..Default::default()
    };
    let versions = remote.space.list_versions(&request).unwrap().versions;
    assert_eq!(versions.len(), 2);
    let first = versions[1].version_id.clone();

    let id = ask(&site, &RestoreIntent::version(node, first).unwrap());
    let step = a.folder.requested();
    a.take(step).unwrap();
    assert_eq!(a.run(&mut remote), ["send", "stage"]);
    assert_eq!(kinds(a.sent.last().unwrap()), ["restore"]);
    assert_eq!(
        files(&site.root),
        [("notes.txt".to_owned(), "the first words".to_owned())]
    );
    assert_eq!(ended(&site), [(id, Outcome::Restored)]);

    a.settle();
    assert!(a.work.is_empty());
    assert_eq!(a.sent.len(), 3);
    assert_eq!(a.state(), FolderState::UpToDate);
}

#[test]
fn a_folder_with_a_queued_restore_is_not_up_to_date() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("notes.txt"), b"written on a");
    assert_eq!(a.state(), FolderState::UpToDate);

    let node = node_named(&a, "notes.txt");
    let intent = RestoreIntent::from_trash(node, None, None).unwrap();
    let commits = &mut a.folder.engine.commits;
    assert!(commits.restore(crate::restore::new_id(NOW as u64), intent));
    assert_eq!(a.state(), FolderState::CatchingUp);
}
