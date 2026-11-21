// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_store_put_back_while_the_folder_runs_across_an_s_only_pull_brings_nothing_back() {
    for commit in [false, true] {
        let mut remote = Remote::new();
        let (site, mut a) = linked(&mut remote, "a");
        let (other, mut b) = linked(&mut remote, "b");
        fs::write(other.root.join("f.txt"), b"A").unwrap();
        sync(&mut remote, (&site, &mut a), (&other, &mut b));
        let copy = site.db.with_extension("copy");
        copy_store_while_open(&site.db, &copy);

        fs::write(other.root.join("f.txt"), b"B").unwrap();
        b.rescan();
        b.rest(&mut remote);
        let marker = || koidrive_fs::read_marker(&site.root).unwrap();
        let (sent, before) = (a.sent.len(), marker());
        a.connect(&remote);
        fs::write(site.root.join("f.txt"), b"B").unwrap();
        a.rescan();
        a.settle();
        a.pull(&remote);
        a.rest(&mut remote);
        assert_eq!((a.sent.len(), marker()), (sent, before), "{commit}");

        let then = match commit {
            true => {
                a.create(&mut remote, &site.root.join("h.txt"), b"h");
                "h.txt"
            }
            false => {
                b.create(&mut remote, &other.root.join("g.txt"), b"g");
                a.pull(&remote);
                a.rest(&mut remote);
                "g.txt"
            }
        };
        assert_eq!(names(&site.root), ["f.txt", then], "{commit}");
        copy_store_while_open(&copy, &site.db);

        fs::remove_file(other.root.join("f.txt")).unwrap();
        b.rescan();
        b.rest(&mut remote);
        let records = remote.space.resume(&a.since()).unwrap();
        let stop = a.folder.event(Event::Changes(records)).err();
        assert!(
            matches!(stop, Some(Stop::StoreBehind)),
            "{commit}: {stop:?}"
        );
        drop(a);

        let mut a = site.start();
        let halted = a.folder.engine.commits.halted();
        assert!(
            matches!(halted, Some(CommitHalt::Recovery { .. })),
            "{commit}"
        );
        a.connect(&remote);
        a.settle();
        a.rest(&mut remote);
        let [question] = &shown(&site)[..] else {
            panic!("{commit}: one question");
        };
        let held: Vec<_> = (question.items.iter())
            .map(|item| item.path.unquoted())
            .collect();
        assert_eq!(held, ["f.txt"], "{commit}");
        drop(a);

        answer(&site, Answer::Discard);
        let mut a = restarted(&site, &mut remote);
        a.rest(&mut remote);
        assert_eq!(a.folder.view().questions, 0, "{commit}");
        assert_eq!(remote.names(), [then], "{commit}");
        assert_eq!(names(&site.root), [then], "{commit}");
        let trash = (site.root.join(koidrive_fs::PRIVATE_DIR)).join(koidrive_fs::TRASH_DIR);
        let trashed = files(&trash);
        assert!(
            trashed.iter().any(|(_, text)| text == "B"),
            "{commit}: {trashed:?}"
        );
    }
}
