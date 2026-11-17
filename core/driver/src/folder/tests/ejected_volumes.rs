// SPDX-License-Identifier: Apache-2.0

fn on_the_disk(at: &Path, files: &[&str]) {
    for name in files {
        let path = at.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, name.as_bytes()).unwrap();
    }
}

fn observed(device: &mut Device, remote: &mut Remote) {
    device.rescan();
    device.rest(remote);
}

fn user_files(remote: &Remote) -> BTreeMap<String, String> {
    (remote.tree().into_iter())
        .filter(|(path, _)| !koidrive_test_support::is_volume_metadata(Path::new(path)))
        .collect()
}

fn held(device: &Device) -> Vec<(String, HeldDiskKind)> {
    (device.folder.view().held_disks.into_iter())
        .map(|disk| (disk.path.into(), disk.kind))
        .collect()
}

fn with_disk(
    disks: &mut koidrive_test_support::Disks,
    remote: &mut Remote,
    mount: &str,
) -> (Site, Device, PathBuf) {
    let (site, mut a) = linked(remote, "a");
    let image = disks.image(&mount.replace('/', "-"));
    let at = site.root.join(mount);
    fs::create_dir_all(&at).unwrap();
    disks.attach(&image, &at);
    on_the_disk(&at, &["report.docx", "Slides/deck.key"]);
    on_the_disk(&site.root, &["notes.txt"]);
    observed(&mut a, remote);
    let tree = remote.tree();
    for path in ["report.docx", "Slides/deck.key"] {
        assert!(tree.contains_key(&format!("{mount}/{path}")), "{tree:?}");
    }
    (site, a, image)
}

#[test]
fn a_disk_ejected_while_the_folder_syncs_leaves_the_server_as_it_was() {
    let Some(mut disks) = koidrive_test_support::Disks::new() else {
        return;
    };
    let mut remote = Remote::new();
    for mount in ["Disk", "Projects/2026/Disk"] {
        let (site, mut a, image) = with_disk(&mut disks, &mut remote, mount);
        let tree = user_files(&remote);
        assert_eq!(held(&a), []);
        disks.detach(&site.root.join(mount));
        observed(&mut a, &mut remote);
        assert_eq!(user_files(&remote), tree, "{mount}");
        assert_eq!(held(&a), [(mount.to_owned(), HeldDiskKind::NotConnected)]);
        fs::remove_dir(site.root.join(mount)).unwrap();
        observed(&mut a, &mut remote);
        assert_eq!(user_files(&remote), tree, "{mount}");

        fs::write(site.root.join("notes.txt"), b"edited").unwrap();
        on_the_disk(&site.root, &["after.txt"]);
        observed(&mut a, &mut remote);
        let after = user_files(&remote);
        assert!(after.contains_key("after.txt"));
        assert_eq!(after.len(), tree.len() + 1, "{mount}: {after:?}");

        fs::create_dir(site.root.join(mount)).unwrap();
        disks.attach(&image, &site.root.join(mount));
        observed(&mut a, &mut remote);
        assert_eq!(held(&a), []);
        assert_eq!(user_files(&remote), after, "{mount}");
        disks.detach(&site.root.join(mount));
        remote = Remote::new();
    }
}

#[test]
fn a_disk_ejected_while_the_folder_is_stopped_leaves_the_server_as_it_was() {
    let Some(mut disks) = koidrive_test_support::Disks::new() else {
        return;
    };
    let mut remote = Remote::new();
    let (site, a, image) = with_disk(&mut disks, &mut remote, "Disk");
    let tree = user_files(&remote);
    let at = site.root.join("Disk");
    drop(a);
    disks.detach(&at);
    let mut a = restarted(&site, &mut remote);
    a.rest(&mut remote);
    assert_eq!(user_files(&remote), tree);
    drop(a);

    fs::remove_dir(&at).unwrap();
    let mut a = restarted(&site, &mut remote);
    a.rest(&mut remote);
    assert_eq!(user_files(&remote), tree);
    assert!(!at.exists(), "nothing is downloaded where the disk was");
    drop(a);

    fs::create_dir(&at).unwrap();
    disks.attach(&image, &at);
    let mut a = restarted(&site, &mut remote);
    a.rest(&mut remote);
    assert_eq!(user_files(&remote), tree);
    on_the_disk(&at, &["new.txt"]);
    observed(&mut a, &mut remote);
    assert!(remote.tree().contains_key("Disk/new.txt"));
}

fn another_disk_where_one_was(first: &Path, second: &Path, stopped: bool) {
    let mut disks = koidrive_test_support::Disks::new().unwrap();
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let at = site.root.join("Disk");
    fs::create_dir_all(&at).unwrap();
    disks.attach(first, &at);
    on_the_disk(&at, &["report.docx", "Slides/deck.key"]);
    observed(&mut a, &mut remote);
    let tree = user_files(&remote);
    assert!(tree.contains_key("Disk/report.docx"), "{tree:?}");
    if stopped {
        drop(a);
        disks.detach(&at);
        disks.attach(second, &at);
        on_the_disk(&at, &["other.txt"]);
        a = restarted(&site, &mut remote);
        a.rest(&mut remote);
    } else {
        disks.detach(&at);
        observed(&mut a, &mut remote);
        disks.attach(second, &at);
        on_the_disk(&at, &["other.txt"]);
        observed(&mut a, &mut remote);
    }
    assert_eq!(user_files(&remote), tree, "{first:?} stopped={stopped}");
    assert_eq!(held(&a), [("Disk".to_owned(), HeldDiskKind::NotConnected)]);
    disks.detach(&at);
}

#[test]
fn another_disk_mounted_where_one_was_leaves_the_first_disks_files_on_the_server() {
    let Some(disks) = koidrive_test_support::Disks::new() else {
        return;
    };
    let mut pairs = vec![(disks.image("First"), disks.image("Second"))];
    if cfg!(target_os = "macos") {
        pairs.push((disks.fat("FATONE"), disks.fat("FATTWO")));
    }
    #[cfg(target_os = "macos")]
    pairs.push((disks.exfat("EXONE"), disks.exfat("EXTWO")));
    for (first, second) in &pairs {
        for stopped in [false, true] {
            another_disk_where_one_was(first, second, stopped);
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn a_disk_without_a_uuid_is_never_synced() {
    let Some(mut disks) = koidrive_test_support::Disks::new() else {
        return;
    };
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let at = site.root.join("Disk");
    fs::create_dir_all(&at).unwrap();
    let stick = disks.fat("Stick");
    disks.attach(&stick, &at);
    on_the_disk(&at, &["report.docx"]);
    on_the_disk(&site.root, &["notes.txt"]);
    observed(&mut a, &mut remote);
    let tree = remote.tree();
    assert!(tree.contains_key("notes.txt"), "{tree:?}");
    assert!(!tree.keys().any(|path| path.starts_with("Disk/")), "{tree:?}");
    assert_eq!(held(&a), [("Disk".to_owned(), HeldDiskKind::NotConnected)]);
}

#[test]
fn a_disk_mounted_over_synced_files_leaves_them_on_the_server_until_it_is_ejected() {
    let Some(mut disks) = koidrive_test_support::Disks::new() else {
        return;
    };
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    on_the_disk(&site.root, &["Data/keep.docx"]);
    observed(&mut a, &mut remote);
    let tree = user_files(&remote);
    let at = site.root.join("Data");
    let image = disks.image("Over");
    disks.attach(&image, &at);
    on_the_disk(&at, &["on-the-disk.txt"]);
    observed(&mut a, &mut remote);
    assert_eq!(user_files(&remote), tree);
    assert_eq!(held(&a), [("Data".to_owned(), HeldDiskKind::MountedOver)]);

    disks.detach(&at);
    observed(&mut a, &mut remote);
    assert_eq!(user_files(&remote), tree);
    assert_eq!(held(&a), []);
    on_the_disk(&at, &["after.txt"]);
    observed(&mut a, &mut remote);
    let after = user_files(&remote);
    assert!(after.contains_key("Data/after.txt"), "{after:?}");
    assert_eq!(after["Data/keep.docx"], tree["Data/keep.docx"]);
}

#[test]
fn two_copies_of_one_disk_image_mounted_at_once_keep_their_own_files() {
    let Some(mut disks) = koidrive_test_support::Disks::new() else {
        return;
    };
    let one = disks.image("One");
    let staging = disks.base().join("staging");
    fs::create_dir(&staging).unwrap();
    disks.attach(&one, &staging);
    on_the_disk(&staging, &["S/f.txt", "S/g.txt"]);
    disks.detach(&staging);
    let two = disks.copy(&one, "Two");

    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::create_dir(site.root.join("A")).unwrap();
    disks.attach(&one, &site.root.join("A"));
    observed(&mut a, &mut remote);
    let before = remote.tree();

    fs::remove_file(site.root.join("A/S/f.txt")).unwrap();
    fs::create_dir(site.root.join("B")).unwrap();
    disks.attach(&two, &site.root.join("B"));
    observed(&mut a, &mut remote);
    let after = remote.tree();
    assert!(!after.contains_key("A/S/f.txt"), "{after:?}");
    assert_eq!(after["A/S/g.txt"], before["A/S/g.txt"], "{after:?}");
    for path in ["B/S/f.txt", "B/S/g.txt"] {
        assert!(after.contains_key(path), "{after:?}");
    }
    assert!(!before.values().any(|node| after.get("B/S/f.txt") == Some(node)));

    fs::rename(site.root.join("B/S/g.txt"), site.root.join("B/S/h.txt")).unwrap();
    fs::remove_file(site.root.join("A/S/g.txt")).unwrap();
    observed(&mut a, &mut remote);
    let last = remote.tree();
    assert_eq!(last.get("B/S/h.txt"), after.get("B/S/g.txt"), "{last:?}");
    assert!(!last.contains_key("A/S/g.txt"), "{last:?}");
}

fn upgraded_with_a_disk_mounted(stopped: bool) {
    let Some(mut disks) = koidrive_test_support::Disks::new() else {
        return;
    };
    let mut remote = Remote::new();
    let (site, a, _image) = with_disk(&mut disks, &mut remote, "Disk");
    let tree = user_files(&remote);
    drop(a);
    let mut store = Store::open(&site.db).unwrap();
    store
        .transaction(|tx| tx.set_volume_mounts(&BTreeMap::new()))
        .unwrap();
    drop(store);
    let mut a = restarted(&site, &mut remote);
    a.rest(&mut remote);
    assert_eq!(user_files(&remote), tree);
    assert_eq!(held(&a), []);

    let at = site.root.join("Disk");
    if stopped {
        drop(a);
        disks.detach(&at);
        a = restarted(&site, &mut remote);
        a.rest(&mut remote);
    } else {
        disks.detach(&at);
        observed(&mut a, &mut remote);
    }
    assert_eq!(user_files(&remote), tree);
    assert_eq!(held(&a), [("Disk".to_owned(), HeldDiskKind::NotConnected)]);
}

#[test]
fn a_disk_mounted_before_an_upgrade_and_ejected_while_the_folder_syncs_stays_on_the_server() {
    upgraded_with_a_disk_mounted(false);
}

#[test]
fn a_disk_mounted_before_an_upgrade_and_ejected_while_the_folder_is_stopped_stays_on_the_server() {
    upgraded_with_a_disk_mounted(true);
}

#[test]
fn a_folder_moved_above_a_disk_mounted_over_synced_files_keeps_them_on_the_server() {
    let Some(mut disks) = koidrive_test_support::Disks::new() else {
        return;
    };
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    on_the_disk(&site.root, &["p/Data/keep.docx"]);
    observed(&mut a, &mut remote);
    let keep = user_files(&remote)["p/Data/keep.docx"].clone();
    let image = disks.image("Over");
    disks.attach(&image, &site.root.join("p/Data"));
    on_the_disk(&site.root.join("p/Data"), &["on-the-disk.txt"]);
    observed(&mut a, &mut remote);
    fs::rename(site.root.join("p"), site.root.join("p2")).unwrap();
    disks.moved(&site.root.join("p"), &site.root.join("p2"));
    observed(&mut a, &mut remote);
    let synced = |remote: &Remote, at: &str, gone: &str| {
        let files = user_files(remote);
        assert_eq!(files.get(&format!("{at}/Data/keep.docx")), Some(&keep), "{files:?}");
        assert!(
            !(files.keys()).any(|path| path.starts_with(gone) || path.ends_with("on-the-disk.txt")),
            "{files:?}"
        );
    };
    synced(&remote, "p2", "p/");
    assert_eq!(held(&a), [("p2/Data".to_owned(), HeldDiskKind::MountedOver)]);

    fs::rename(site.root.join("p2"), site.root.join("p")).unwrap();
    disks.moved(&site.root.join("p2"), &site.root.join("p"));
    disks.detach(&site.root.join("p/Data"));
    observed(&mut a, &mut remote);
    synced(&remote, "p", "p2/");
    assert_eq!(held(&a), []);
}
