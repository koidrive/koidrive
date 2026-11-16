// SPDX-License-Identifier: Apache-2.0

fn synced_and_stopped(remote: &mut Remote) -> Site {
    let (site, mut a) = linked(remote, "a");
    fs::create_dir(site.root.join("Slides")).unwrap();
    a.create(remote, &site.root.join("Slides/deck.key"), b"deck");
    a.create(remote, &site.root.join("Report.docx"), b"report");
    a.create(remote, &site.root.join("budget.xlsx"), b"budget");
    a.rest(remote);
    site
}

fn rewrite_link(site: &Site, change: impl FnOnce(&mut Link)) {
    let mut store = Store::open(&site.db).unwrap();
    store
        .transaction(|tx| {
            let mut link = tx.link()?.unwrap();
            change(&mut link);
            tx.set_link(&link)
        })
        .unwrap();
}

fn stored_link(site: &Site) -> Link {
    let mut store = Store::open(&site.db).unwrap();
    store.transaction(|tx| tx.link()).unwrap().unwrap()
}

fn admit(site: &Site) -> Result<Option<Adopted>, Stop> {
    let mut link = site.link.clone();
    disk::admit_root(&mut link, &mut Store::open(&site.db).unwrap())
}

fn assert_unchanged(site: &Site, remote: &Remote, tree: &BTreeMap<String, String>, a: &Device) {
    assert_eq!(&remote.tree(), tree);
    assert!(a.sent.is_empty(), "nothing is sent: {:?}", a.sent);
    assert!(a.folder.engine.trees.dirty().is_empty());
    assert_eq!(a.folder.view().problem, None);
    assert_eq!(names(&site.root), ["Report.docx", "Slides", "budget.xlsx"]);
    assert_eq!(names(&site.root.join("Slides")), ["deck.key"]);
}

#[test]
fn an_os_update_that_renumbers_the_disk_resumes_with_nothing_removed_created_or_copied() {
    let mut remote = Remote::new();
    let site = synced_and_stopped(&mut remote);
    let tree = remote.tree();
    let found = found_root(&site.root);
    rewrite_link(&site, |link| link.root_dev = found.dev.wrapping_add(3));

    if found.volume.is_none() {
        assert!(matches!(
            admit(&site),
            Err(Stop::Root(RootProblem::Renumbered))
        ));
        assert_eq!(stored_link(&site).root_dev, found.dev.wrapping_add(3));
        return;
    }
    assert!(matches!(admit(&site), Ok(Some(Adopted::Volume))));
    let mut a = restarted(&site, &mut remote);
    a.rest(&mut remote);

    assert_unchanged(&site, &remote, &tree, &a);
    assert_eq!(stored_link(&site).root_at(), found);
    assert!(matches!(admit(&site), Ok(None)));
}

#[test]
fn a_link_from_before_volume_identities_adopts_its_renumbered_disk_by_the_entries_in_it() {
    let mut remote = Remote::new();
    let site = synced_and_stopped(&mut remote);
    let tree = remote.tree();
    let found = found_root(&site.root);
    rewrite_link(&site, |link| {
        link.root_dev = found.dev.wrapping_add(3);
        link.root_volume = None;
    });

    if found.volume.is_none() {
        assert!(matches!(
            admit(&site),
            Err(Stop::Root(RootProblem::Renumbered))
        ));
        return;
    }
    assert!(matches!(admit(&site), Ok(Some(Adopted::Children(3)))));
    let mut a = restarted(&site, &mut remote);
    a.rest(&mut remote);

    assert_unchanged(&site, &remote, &tree, &a);
    assert_eq!(stored_link(&site).root_at(), found);
}

#[test]
fn a_link_from_before_volume_identities_keeps_one_gone_entry_out_of_the_count() {
    let mut remote = Remote::new();
    let site = synced_and_stopped(&mut remote);
    let found = found_root(&site.root);
    if found.volume.is_none() {
        return;
    }
    fs::remove_file(site.root.join("budget.xlsx")).unwrap();
    rewrite_link(&site, |link| {
        link.root_dev = found.dev.wrapping_add(3);
        link.root_volume = None;
    });

    assert!(matches!(admit(&site), Ok(Some(Adopted::Children(2)))));
    let mut a = restarted(&site, &mut remote);
    a.rest(&mut remote);

    assert!(!remote.tree().contains_key("budget.xlsx"));
    assert!(remote.tree().contains_key("Report.docx"));
}

#[test]
fn a_link_from_before_volume_identities_halts_on_a_copy_of_its_folder() {
    let mut remote = Remote::new();
    let site = synced_and_stopped(&mut remote);
    let tree = remote.tree();
    let report = site.root.join("Report.docx");
    fs::copy(&report, site.root.join("copy")).unwrap();
    fs::rename(site.root.join("copy"), &report).unwrap();
    let found = found_root(&site.root);
    rewrite_link(&site, |link| {
        link.root_dev = found.dev.wrapping_add(3);
        link.root_volume = None;
    });

    assert!(matches!(
        admit(&site),
        Err(Stop::Root(RootProblem::Renumbered))
    ));
    assert_eq!(remote.tree(), tree);
    assert_eq!(stored_link(&site).root_dev, found.dev.wrapping_add(3));
}

#[test]
fn a_link_from_before_volume_identities_halts_when_every_entry_in_it_is_gone() {
    let mut remote = Remote::new();
    let site = synced_and_stopped(&mut remote);
    let tree = remote.tree();
    fs::remove_dir_all(site.root.join("Slides")).unwrap();
    fs::remove_file(site.root.join("Report.docx")).unwrap();
    fs::remove_file(site.root.join("budget.xlsx")).unwrap();
    let found = found_root(&site.root);
    rewrite_link(&site, |link| {
        link.root_dev = found.dev.wrapping_add(3);
        link.root_volume = None;
    });

    assert!(matches!(
        admit(&site),
        Err(Stop::Root(RootProblem::Renumbered))
    ));
    assert_eq!(remote.tree(), tree);
}

#[test]
fn a_folder_on_another_volume_halts() {
    let mut remote = Remote::new();
    let site = synced_and_stopped(&mut remote);
    let tree = remote.tree();
    let found = found_root(&site.root);
    rewrite_link(&site, |link| link.root_volume = Some(VolumeId::Uuid([7; 16])));
    assert!(matches!(admit(&site), Err(Stop::Root(RootProblem::OtherDisk))));
    assert_eq!(stored_link(&site).root_at().dev, found.dev);
    assert_eq!(remote.tree(), tree);
}

#[test]
fn a_folder_locked_under_another_device_number_is_the_same_folder() {
    let dir = tempfile::tempdir().unwrap();
    let meta = fs::metadata(dir.path()).unwrap();
    let now = Identity::of(&meta);
    let mut recorded = serde_json::to_value(now).unwrap();
    recorded["dev"] = serde_json::json!(meta.dev() + 3);
    assert_eq!(serde_json::from_value::<Identity>(recorded).unwrap(), now);
}

#[test]
fn a_restart_right_after_an_adoption_and_a_second_renumbering_converges() {
    let mut remote = Remote::new();
    let site = synced_and_stopped(&mut remote);
    let tree = remote.tree();
    let found = found_root(&site.root);
    if found.volume.is_none() {
        return;
    }
    rewrite_link(&site, |link| {
        link.root_dev = found.dev.wrapping_add(3);
        link.root_volume = None;
    });
    assert!(matches!(admit(&site), Ok(Some(Adopted::Children(3)))));
    assert_eq!(stored_link(&site).root_at(), found);

    rewrite_link(&site, |link| link.root_dev = found.dev.wrapping_add(5));
    assert!(matches!(admit(&site), Ok(Some(Adopted::Volume))));
    let mut a = restarted(&site, &mut remote);
    a.rest(&mut remote);

    assert_unchanged(&site, &remote, &tree, &a);
    assert_eq!(stored_link(&site).root_at(), found);
}
