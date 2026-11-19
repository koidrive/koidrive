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
        // The create, the delete and one restore.
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

#[test]
fn a_restore_asked_for_before_a_rewind_is_shown_by_its_file_and_answered_by_its_request() {
    let mut remote = Remote::new();
    let (site, mut a, trashed) = with_a_trashed_file(&mut remote);
    let path = site.root.join("draft.txt");
    a.create(&mut remote, &path, b"the first words");
    fs::write(&path, b"and the second").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    let node = node_named(&a, "draft.txt");
    let request = ListVersions {
        node_id: node.clone(),
        ..Default::default()
    };
    let first = remote.space.list_versions(&request).unwrap().versions[1]
        .version_id
        .clone();
    let backup = remote.space.clone();
    drop(a);
    let unknown = || crate::restore::new_id(NOW as u64);
    let asked = [
        (
            RestoreIntent::version(node, first).unwrap(),
            Some("draft.txt"),
        ),
        (
            RestoreIntent::from_trash(trashed, None, None).unwrap(),
            Some("notes.txt"),
        ),
        (
            RestoreIntent::from_trash(unknown(), None, Some("gone.txt".into())).unwrap(),
            Some("gone.txt"),
        ),
        (
            RestoreIntent::from_trash(unknown(), None, None).unwrap(),
            None,
        ),
    ];
    let mut expected: Vec<(String, String, u64)> = (asked.iter())
        .map(|(intent, path)| {
            let id = ask(&site, intent);
            (id.clone(), path.map_or(id, str::to_owned), 1)
        })
        .collect();
    expected.sort_by(|a, b| a.1.cmp(&b.1));
    remote.space.rewind(&backup, NOW);

    let requested = |site: &Site| {
        (shown(site).into_iter())
            .find(|question| {
                matches!(
                    question.kind,
                    crate::question::QuestionKind::Recovery {
                        group: crate::question::RecoveryGroup::RequestedBeforeReset,
                        ..
                    }
                )
            })
            .expect("a question holds the restores")
    };
    let items = |question: &crate::question::Question| -> Vec<(String, String, u64)> {
        (question.items.iter())
            .map(|item| {
                (
                    item.node_id.clone(),
                    item.path.unquoted().to_owned(),
                    item.files,
                )
            })
            .collect()
    };
    let mut a = restarted(&site, &mut remote);
    a.rest(&mut remote);
    drop(a);
    let held = requested(&site);
    assert_eq!(items(&held), expected);

    let mut blank = held.clone();
    for item in &mut blank.items {
        item.path = "".into();
        item.files = 0;
    }
    (Store::open(&site.db).unwrap())
        .transaction(|tx| tx.reshow_question(&held.id, &serde_json::to_vec(&blank).unwrap()))
        .unwrap();
    restarted(&site, &mut remote).rest(&mut remote);
    assert_eq!(
        items(&requested(&site)),
        expected,
        "a row an older build stored"
    );

    let given = koidrive_store::GivenAnswer {
        answer: Answer::Cancel.as_str().into(),
        answered_at_ms: NOW,
    };
    (Store::open(&site.db).unwrap())
        .transaction(|tx| tx.answer_question(&held.id, &given))
        .unwrap();
    restarted(&site, &mut remote).rest(&mut remote);
    let cancelled = Outcome::Rejected(koidrive_engine::restore::Rejection::Cancelled);
    let mut ended = ended(&site);
    ended.sort_by(|a, b| a.0.cmp(&b.0));
    let mut ids: Vec<String> = expected.into_iter().map(|(id, _, _)| id).collect();
    ids.sort();
    assert_eq!(
        ended,
        ids.into_iter()
            .map(|id| (id, cancelled.clone()))
            .collect::<Vec<_>>()
    );
}

fn restored_after_a_big_deletion(folders: &[&str], count: usize, removed: &str) -> (Site, Remote) {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    for folder in folders {
        let dir = site.root.join(folder);
        fs::create_dir_all(&dir).unwrap();
        for i in 0..count {
            fs::write(dir.join(format!("f{i:05}.txt")), format!("{i}")).unwrap();
        }
    }
    a.rescan();
    a.rest(&mut remote);
    let node = node_named(&a, removed.rsplit('/').next().unwrap());
    fs::remove_dir_all(site.root.join(removed)).unwrap();
    a.rescan();
    a.rest(&mut remote);
    drop(a);
    answer(&site, Answer::Delete);
    restarted(&site, &mut remote).rest(&mut remote);
    assert_eq!(remote.names(), Vec::<String>::new());

    ask(&site, &RestoreIntent::from_trash(node, None, None).unwrap());
    restarted(&site, &mut remote).rest(&mut remote);
    (site, remote)
}

#[test]
fn a_restored_folder_brings_back_every_file_its_deletion_trashed_over_several_commits() {
    let (site, remote) = restored_after_a_big_deletion(&["big"], 2000, "big");
    assert_eq!(files(&site.root).len(), 2000);
    assert_eq!(remote.names().len(), 2001);
}

#[test]
fn a_restored_folder_brings_back_the_folders_its_deletion_trashed_on_their_own() {
    let folders = ["big/a", "big/b", "big/c", "big/d"];
    let (site, remote) = restored_after_a_big_deletion(&folders, 500, "big");
    assert_eq!(files(&site.root).len(), 2000);
    assert_eq!(remote.names().len(), 2005);
}

#[test]
fn a_restored_folder_brings_back_a_big_folder_inside_it() {
    let (site, remote) = restored_after_a_big_deletion(&["outer/inner"], 2000, "outer");
    assert_eq!(files(&site.root).len(), 2000);
    assert_eq!(remote.names().len(), 2002);
}

#[test]
fn a_folder_restore_asked_for_before_a_rewind_shows_as_the_folder_with_the_files_trashed_with_it() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::create_dir(site.root.join("photos")).unwrap();
    for name in ["a.jpg", "b.jpg"] {
        fs::write(site.root.join("photos").join(name), name).unwrap();
    }
    a.scan_until(|a| !a.work.is_empty());
    a.rest(&mut remote);
    let photos = node_named(&a, "photos");
    fs::remove_dir_all(site.root.join("photos")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert_eq!(a.run(&mut remote), ["send"]);
    let backup = remote.space.clone();
    drop(a);
    let id = ask(&site, &RestoreIntent::from_trash(photos, None, None).unwrap());
    remote.space.rewind(&backup, NOW);
    restarted(&site, &mut remote).rest(&mut remote);

    let question = (shown(&site).into_iter())
        .find(|question| !question.items.is_empty())
        .expect("a question holds the restore");
    let items: Vec<_> = (question.items.iter())
        .map(|item| {
            let kind = serde_json::to_value(item).unwrap()["kind"].clone();
            (item.node_id.as_str(), item.path.unquoted(), kind, item.files)
        })
        .collect();
    assert_eq!(items, [(id.as_str(), "photos", serde_json::json!("folder"), 2)]);
}
