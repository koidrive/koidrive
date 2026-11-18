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

fn identified(at: &Path) -> RootAt {
    let started = std::time::Instant::now();
    loop {
        let found = found_root(at);
        if found.volume.is_some() || started.elapsed() > std::time::Duration::from_secs(10) {
            return found;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn with_disk(
    disks: &mut koidrive_test_support::Disks,
    remote: &mut Remote,
    mount: &str,
    image: PathBuf,
) -> (Site, Device, PathBuf) {
    let (site, mut a) = linked(remote, "a");
    let at = site.root.join(mount);
    fs::create_dir_all(&at).unwrap();
    disks.attach(&image, &at);
    identified(&at);
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
    let mut disk_images = vec![
        ("Disk", disks.image("Disk")),
        ("Projects/2026/Disk", disks.image("Projects-2026-Disk")),
        ("Stick", disks.fat("Stick")),
    ];
    if disks.exfat_has_ids() {
        disk_images.push(("Card", disks.exfat("Card")));
    }
    for (mount, image) in disk_images {
        let (site, mut a, image) = with_disk(&mut disks, &mut remote, mount, image);
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
        identified(&site.root.join(mount));
        observed(&mut a, &mut remote);
        assert_eq!(held(&a), [], "{mount}");
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
    let mut images = vec![disks.image("Disk"), disks.fat("Stick")];
    if disks.exfat_has_ids() {
        images.push(disks.exfat("Card"));
    }
    for image in images {
        ejected_while_stopped(&mut disks, image);
    }
}

fn ejected_while_stopped(disks: &mut koidrive_test_support::Disks, image: PathBuf) {
    let mut remote = Remote::new();
    let (site, a, image) = with_disk(disks, &mut remote, "Disk", image);
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
    identified(&at);
    let mut a = restarted(&site, &mut remote);
    a.rest(&mut remote);
    assert_eq!(user_files(&remote), tree);
    on_the_disk(&at, &["new.txt"]);
    observed(&mut a, &mut remote);
    assert_eq!(held(&a), [], "{image:?}");
    assert!(remote.tree().contains_key("Disk/new.txt"), "{image:?}");
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

type Formatted = fn(&koidrive_test_support::Disks, &str, u32, u32) -> PathBuf;

fn serial_of(disks: &koidrive_test_support::Disks) -> u32 {
    use std::hash::{Hash, Hasher};
    let mut serial = std::hash::DefaultHasher::new();
    disks.base().hash(&mut serial);
    serial.finish() as u32 | 1
}

fn recorded_before_sizes(at: &Path, serial: u32) -> VolumeId {
    if cfg!(target_os = "macos") {
        let info = std::process::Command::new("diskutil")
            .args(["info", "-plist"])
            .arg(at)
            .output()
            .unwrap();
        let info = String::from_utf8(info.stdout).unwrap();
        let after = info.split("<key>VolumeUUID</key>").nth(1).unwrap();
        let uuid = after.split("<string>").nth(1).unwrap();
        let hex: String = uuid.chars().take(36).filter(|c| *c != '-').collect();
        let bytes: Vec<u8> = (0..32)
            .step_by(2)
            .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap())
            .collect();
        return VolumeId::Uuid(bytes.try_into().unwrap());
    }
    let mut id = [0; 16];
    id[..4].copy_from_slice(&serial.to_le_bytes());
    id[15] = 4;
    VolumeId::Uuid(id)
}

fn formats(disks: &koidrive_test_support::Disks) -> Vec<(&'static str, Formatted)> {
    let mut formats: Vec<(&str, Formatted)> =
        vec![("FAT", koidrive_test_support::Disks::fat_with_serial)];
    if disks.exfat_has_ids() {
        formats.push(("exFAT", koidrive_test_support::Disks::exfat_with_serial));
    }
    formats
}

fn swapped_for_a_bigger_stick_with_its_serial(
    format: Formatted,
    stopped: bool,
    identical: bool,
) {
    let mut disks = koidrive_test_support::Disks::new().unwrap();
    let serial = serial_of(&disks);
    let first = format(&disks, "First", 16, serial);
    let second = format(&disks, "Second", 32, serial);
    let stamp = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_767_225_600);
    let shared = "README.txt";
    let stage = |at: &Path, body: &str| {
        fs::write(at.join(shared), body).unwrap();
        if identical {
            let file = fs::File::options().write(true).open(at.join(shared)).unwrap();
            file.set_modified(stamp).unwrap();
        }
    };
    let staging = disks.base().join("Staging");
    fs::create_dir(&staging).unwrap();
    disks.attach(&second, &staging);
    stage(&staging, if identical { shared } else { "another stick" });
    on_the_disk(&staging, &["other.txt"]);
    disks.detach(&staging);
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let at = site.root.join("Disk");
    fs::create_dir_all(&at).unwrap();
    disks.attach(&first, &at);
    identified(&at);
    on_the_disk(&at, &["report.docx", "Slides/deck.key"]);
    stage(&at, shared);
    observed(&mut a, &mut remote);
    let tree = user_files(&remote);
    assert!(tree.contains_key("Disk/Slides/deck.key"), "{tree:?}");
    if stopped {
        drop(a);
        disks.detach(&at);
        disks.attach(&second, &at);
        identified(&at);
        a = restarted(&site, &mut remote);
        a.rest(&mut remote);
    } else {
        disks.detach(&at);
        observed(&mut a, &mut remote);
        disks.attach(&second, &at);
        identified(&at);
    }
    observed(&mut a, &mut remote);
    let (after, held) = (user_files(&remote), held(&a));
    drop(a);
    disks.detach(&at);
    assert_eq!(after, tree, "stopped={stopped} identical={identical}");
    assert_eq!(held, [("Disk".to_owned(), HeldDiskKind::NotConnected)]);
}

#[test]
fn a_bigger_stick_with_a_sticks_serial_keeps_the_first_sticks_files() {
    let Some(disks) = koidrive_test_support::Disks::new() else {
        return;
    };
    for (_, format) in formats(&disks) {
        for (stopped, identical) in [(false, false), (true, false), (false, true), (true, true)] {
            swapped_for_a_bigger_stick_with_its_serial(format, stopped, identical);
        }
    }
}

#[test]
fn a_stick_of_the_same_size_with_a_sticks_serial_puts_the_first_sticks_files_in_deleted_files() {
    let Some(mut disks) = koidrive_test_support::Disks::new() else {
        return;
    };
    let serial = serial_of(&disks);
    for (kind, format) in formats(&disks) {
        let (first, second) = (
            format(&disks, &format!("First{kind}"), 16, serial),
            format(&disks, &format!("Second{kind}"), 16, serial),
        );
        let mut remote = Remote::new();
        let (site, mut a, _) = with_disk(&mut disks, &mut remote, "Disk", first);
        let tree = user_files(&remote);
        let at = site.root.join("Disk");
        disks.detach(&at);
        observed(&mut a, &mut remote);
        assert_eq!(user_files(&remote), tree, "{kind}");
        assert_eq!(held(&a), [("Disk".to_owned(), HeldDiskKind::NotConnected)]);
        disks.attach(&second, &at);
        identified(&at);
        observed(&mut a, &mut remote);
        let left: Vec<String> = user_files(&remote).into_keys().collect();
        assert_eq!(left, ["Disk", "notes.txt"], "{kind}");
        let trashed: Vec<String> = (remote.trash_pages(100).into_iter())
            .flat_map(|page| page.nodes)
            .map(|node| node.node_id)
            .collect();
        let missing = [&tree["Disk/report.docx"], &tree["Disk/Slides"]];
        assert!(missing.iter().all(|id| trashed.contains(id)), "{kind}: {trashed:?}");
        drop(a);
        disks.detach(&at);
        let (site_b, mut b) = linked(&mut remote, "b");
        let asked: Vec<String> = (missing.iter())
            .map(|id| ask(&site_b, &RestoreIntent::from_trash((*id).clone(), None, None).unwrap()))
            .collect();
        let step = b.folder.requested();
        b.take(step).unwrap();
        b.run(&mut remote);
        let written = |tree: BTreeMap<String, String>| {
            let apple_double = |path: &String| path.rsplit('/').next().unwrap().starts_with("._");
            (tree.into_iter())
                .filter(|(path, _)| !apple_double(path))
                .collect::<BTreeMap<_, _>>()
        };
        assert_eq!(written(user_files(&remote)), written(tree), "{kind}");
        for name in ["report.docx", "Slides/deck.key"] {
            let on_b = fs::read(site_b.root.join("Disk").join(name)).unwrap();
            assert_eq!(on_b, name.as_bytes(), "{kind}");
        }
        let ended: BTreeMap<String, Outcome> = ended(&site_b).into_iter().collect();
        let expected: BTreeMap<String, Outcome> =
            (asked.into_iter()).map(|id| (id, Outcome::Restored)).collect();
        assert_eq!(ended, expected, "{kind}");
    }
}

#[test]
fn a_stick_emptied_while_it_is_mounted_syncs_the_deletes() {
    let Some(mut disks) = koidrive_test_support::Disks::new() else {
        return;
    };
    for image in [disks.fat("Emptied"), disks.image("EmptiedDrive")] {
        let mut remote = Remote::new();
        let (site, mut a) = linked(&mut remote, "a");
        let at = site.root.join("Disk");
        fs::create_dir_all(&at).unwrap();
        disks.attach(&image, &at);
        on_the_disk(&at, &["report.docx", "Slides/deck.key"]);
        observed(&mut a, &mut remote);
        assert!(user_files(&remote).contains_key("Disk/report.docx"), "{image:?}");
        fs::remove_file(at.join("report.docx")).unwrap();
        fs::remove_dir_all(at.join("Slides")).unwrap();
        observed(&mut a, &mut remote);
        let left: Vec<String> = user_files(&remote).into_keys().collect();
        assert_eq!(left, ["Disk"], "{image:?}");
        assert_eq!(held(&a), [], "{image:?}");
        disks.detach(&at);
    }
}

#[test]
fn a_fat_stick_mounted_at_another_folder_stays_held_where_it_was() {
    let Some(mut disks) = koidrive_test_support::Disks::new() else {
        return;
    };
    let mut remote = Remote::new();
    let image = disks.fat("Stick");
    let (site, mut a, image) = with_disk(&mut disks, &mut remote, "Disk", image);
    let before = user_files(&remote);
    disks.detach(&site.root.join("Disk"));
    let elsewhere = site.root.join("Elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    disks.attach(&image, &elsewhere);
    observed(&mut a, &mut remote);
    let after = user_files(&remote);
    disks.detach(&elsewhere);
    assert!(before.iter().all(|(path, id)| after.get(path) == Some(id)), "{after:?}");
    assert_eq!(held(&a), [("Disk".to_owned(), HeldDiskKind::NotConnected)]);
}

#[test]
fn an_exfat_stick_swapped_for_another_and_back_deletes_nothing() {
    let Some(mut disks) = koidrive_test_support::Disks::new() else {
        return;
    };
    if !disks.exfat_has_ids() {
        return;
    }
    for stopped in [false, true] {
        let mut remote = Remote::new();
        let (one, two) = (disks.exfat(&format!("One{stopped}")), disks.exfat(&format!("Two{stopped}")));
        let (site, mut a, one) = with_disk(&mut disks, &mut remote, "Disk", one);
        let at = site.root.join("Disk");
        let tree = user_files(&remote);
        for (step, image) in [("swapped", Some(&two)), ("back", Some(&one)), ("ejected", None)] {
            if stopped {
                drop(a);
                disks.detach(&at);
                if let Some(image) = image {
                    disks.attach(image, &at);
                    identified(&at);
                }
                a = restarted(&site, &mut remote);
                a.rest(&mut remote);
            } else {
                disks.detach(&at);
                observed(&mut a, &mut remote);
                assert_eq!(user_files(&remote), tree, "stopped={stopped} {step}");
                if let Some(image) = image {
                    disks.attach(image, &at);
                    identified(&at);
                }
            }
            observed(&mut a, &mut remote);
            assert_eq!(user_files(&remote), tree, "stopped={stopped} {step}");
            let expected = match step {
                "back" => vec![],
                _ => vec![("Disk".to_owned(), HeldDiskKind::NotConnected)],
            };
            assert_eq!(held(&a), expected, "stopped={stopped} {step}");
        }
    }
}

#[test]
fn a_stick_changed_on_another_computer_comes_back_with_the_change() {
    let Some(mut disks) = koidrive_test_support::Disks::new() else {
        return;
    };
    let edits = [
        ("report.docx", "the report, edited at work"),
        ("Slides/deck.key", "the deck, edited on another computer"),
    ];
    for (stopped, deleted) in [(false, false), (true, false), (false, true), (true, true)] {
        let name = format!("C{}{}", u8::from(stopped), u8::from(deleted));
        let mut sticks = vec![disks.fat(&name)];
        if disks.exfat_has_ids() {
            sticks.push(disks.exfat(&format!("{name}Card")));
        }
        for stick in sticks {
            let mut remote = Remote::new();
            let (site, mut a, image) = with_disk(&mut disks, &mut remote, "Disk", stick);
            let tree = user_files(&remote);
            let at = site.root.join("Disk");
            disks.detach(&at);
            observed(&mut a, &mut remote);
            let elsewhere = disks.base().join(format!("Another computer {name}"));
            fs::create_dir_all(&elsewhere).unwrap();
            disks.attach(&image, &elsewhere);
            std::thread::sleep(std::time::Duration::from_millis(2100));
            if deleted {
                fs::remove_file(elsewhere.join("report.docx")).unwrap();
                fs::remove_dir_all(elsewhere.join("Slides")).unwrap();
            }
            for (file, text) in edits.iter().filter(|_| !deleted) {
                fs::write(elsewhere.join(file), text).unwrap();
            }
            disks.detach(&elsewhere);
            if stopped {
                drop(a);
                disks.attach(&image, &at);
                identified(&at);
                a = restarted(&site, &mut remote);
                a.rest(&mut remote);
            } else {
                disks.attach(&image, &at);
                identified(&at);
            }
            observed(&mut a, &mut remote);
            let (after, held) = (user_files(&remote), held(&a));
            drop(a);
            disks.detach(&at);
            let case = format!("{image:?} stopped={stopped} deleted={deleted}");
            assert_eq!(held, [], "{case}");
            if deleted {
                let left: Vec<&String> = after.keys().collect();
                assert_eq!(left, ["Disk", "notes.txt"], "{case}");
                continue;
            }
            assert_eq!(after, tree, "{case}");
            for (file, text) in edits {
                let id = &after[&format!("Disk/{file}")];
                assert_eq!(server_bytes(&remote, id), text.as_bytes(), "{case} {file}");
            }
        }
    }
}

#[test]
fn a_stick_recorded_before_its_size_was_comes_back_and_is_told_from_a_bigger_stick_with_its_serial() {
    let Some(mut disks) = koidrive_test_support::Disks::new() else {
        return;
    };
    let serial = serial_of(&disks);
    let cases = (formats(&disks).into_iter()).flat_map(|format| [(format, false), (format, true)]);
    for ((kind, format), unplugged) in cases {
        let (first, second) = (
            format(&disks, &format!("Old{kind}{}", u8::from(unplugged)), 16, serial),
            format(&disks, &format!("Big{kind}{}", u8::from(unplugged)), 32, serial),
        );
        let mut remote = Remote::new();
        let (site, a, _) = with_disk(&mut disks, &mut remote, "Disk", first.clone());
        let tree = user_files(&remote);
        drop(a);
        let at = site.root.join("Disk");
        let (plain, sized) = (recorded_before_sizes(&at, serial), identified(&at).volume.unwrap());
        assert_ne!(plain, sized, "{kind}");
        let mounts = |site: &Site| {
            let mut store = Store::open(&site.db).unwrap();
            store.transaction(|tx| tx.volume_mounts()).unwrap()
        };
        assert_eq!(mounts(&site)[Path::new("Disk")], koidrive_fs::Mount::Volume(sized));
        let mut store = Store::open(&site.db).unwrap();
        let recorded = BTreeMap::from([("Disk".into(), koidrive_fs::Mount::Volume(plain))]);
        store.transaction(|tx| tx.set_volume_mounts(&recorded)).unwrap();
        drop(store);
        if unplugged {
            disks.detach(&at);
        }
        let mut a = restarted(&site, &mut remote);
        a.rest(&mut remote);
        if unplugged {
            disks.attach(&first, &at);
            identified(&at);
            on_the_disk(&at, &["new.txt"]);
            observed(&mut a, &mut remote);
        }
        assert_eq!(held(&a), [], "{kind} {unplugged}");
        let back = user_files(&remote);
        assert!(tree.iter().all(|(path, id)| back.get(path) == Some(id)), "{kind}: {back:?}");
        assert_eq!(back.contains_key("Disk/new.txt"), unplugged, "{kind}");
        assert_eq!(mounts(&site)[Path::new("Disk")], koidrive_fs::Mount::Volume(sized));
        disks.detach(&at);
        observed(&mut a, &mut remote);
        disks.attach(&second, &at);
        identified(&at);
        on_the_disk(&at, &["other.txt"]);
        observed(&mut a, &mut remote);
        let (after, held) = (user_files(&remote), held(&a));
        drop(a);
        disks.detach(&at);
        assert_eq!(after, back, "{kind} {unplugged}");
        assert_eq!(held, [("Disk".to_owned(), HeldDiskKind::NotConnected)], "{kind}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn a_folder_whose_root_is_a_stick_syncs_again_once_the_stick_is_plugged_back_in() {
    let Some(mut disks) = koidrive_test_support::Disks::new() else {
        return;
    };
    let serial = serial_of(&disks);
    let roots: Vec<(PathBuf, bool)> = (formats(&disks).into_iter())
        .flat_map(|(kind, format)| {
            [false, true].map(|recorded| {
                let name = format!("R{kind}{}", u8::from(recorded));
                (format(&disks, &name, 16, serial), recorded)
            })
        })
        .collect();
    for (image, legacy) in roots {
        let mut remote = Remote::new();
        let site = Site::new(&remote, "a");
        disks.attach(&image, &site.root);
        let marker = Marker {
            space_id: site.link.space_id.clone(),
            effects: 0,
            token: None,
        };
        write_marker(&site.root, &marker).unwrap();
        let found = identified(&site.root);
        assert!(found.volume.is_some(), "{image:?}");
        rewrite_link(&site, |link| {
            link.root_dev = found.dev;
            link.root_volume = match legacy {
                true => Some(recorded_before_sizes(&site.root, serial)),
                false => found.volume,
            };
        });
        let mut a = restarted(&site, &mut remote);
        on_the_disk(&site.root, &["report.docx"]);
        observed(&mut a, &mut remote);
        let tree = user_files(&remote);
        assert!(tree.contains_key("report.docx"), "{image:?}: {tree:?}");
        drop(a);
        let mut store = Store::open(&site.db).unwrap();
        let stored = store.transaction(|tx| tx.link()).unwrap().unwrap();
        assert_eq!(stored.root_volume, found.volume, "{image:?}");
        drop(store);
        disks.detach(&site.root);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        disks.attach(&image, &site.root);
        identified(&site.root);
        let mut a = restarted(&site, &mut remote);
        a.rest(&mut remote);
        on_the_disk(&site.root, &["new.txt"]);
        observed(&mut a, &mut remote);
        let (after, held) = (user_files(&remote), held(&a));
        drop(a);
        disks.detach(&site.root);
        assert_eq!(held, [], "{image:?}");
        assert!(after.contains_key("new.txt"), "{image:?}: {after:?}");
        assert_eq!(after.len(), tree.len() + 1, "{image:?}: {after:?}");
    }
}

#[test]
fn another_disk_mounted_where_one_was_leaves_the_first_disks_files_on_the_server() {
    let Some(disks) = koidrive_test_support::Disks::new() else {
        return;
    };
    let pairs = [
        (disks.image("First"), disks.image("Second")),
        (disks.fat("FATONE"), disks.fat("FATTWO")),
        #[cfg(target_os = "macos")]
        (disks.exfat("EXONE"), disks.exfat("EXTWO")),
    ];
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
    let stick = disks.fat_without_serial("Stick");
    disks.attach(&stick, &at);
    on_the_disk(&at, &["report.docx"]);
    on_the_disk(&site.root, &["notes.txt"]);
    observed(&mut a, &mut remote);
    let tree = remote.tree();
    assert!(tree.contains_key("notes.txt"), "{tree:?}");
    assert!(!tree.keys().any(|path| path.starts_with("Disk/")), "{tree:?}");
    assert_eq!(held(&a), [("Disk".to_owned(), HeldDiskKind::NotConnected)]);
}

#[cfg(target_os = "linux")]
#[test]
fn a_disk_without_a_uuid_ejected_stays_held_until_another_disk_takes_its_place() {
    let Some(mut disks) = koidrive_test_support::Disks::new() else {
        return;
    };
    for (stopped, restarts) in [(false, false), (false, true), (true, true)] {
        let case = format!("stopped={stopped} restarts={restarts}");
        let mut remote = Remote::new();
        let (site, mut a) = linked(&mut remote, "a");
        let at = site.root.join("Disk");
        fs::create_dir_all(&at).unwrap();
        let (unknown, stick) = (
            disks.fat_without_serial(&format!("Unknown{stopped}{restarts}")),
            disks.fat(&format!("Stick{stopped}{restarts}")),
        );
        disks.attach(&unknown, &at);
        on_the_disk(&at, &["report.docx"]);
        on_the_disk(&site.root, &["notes.txt"]);
        observed(&mut a, &mut remote);
        let tree = user_files(&remote);
        assert!(tree.contains_key("notes.txt"), "{tree:?}");
        assert!(!tree.contains_key("Disk/report.docx"), "{tree:?}");
        assert_eq!(held(&a), [("Disk".to_owned(), HeldDiskKind::NotConnected)]);
        if !stopped {
            disks.detach(&at);
            observed(&mut a, &mut remote);
        }
        if restarts {
            drop(a);
            if stopped {
                disks.detach(&at);
            }
            a = restarted(&site, &mut remote);
            a.rest(&mut remote);
            observed(&mut a, &mut remote);
        }
        assert_eq!(user_files(&remote), tree, "{case}");
        assert_eq!(held(&a), [("Disk".to_owned(), HeldDiskKind::NotConnected)], "{case}");
        assert!(at.is_dir(), "{case}");
        assert_eq!(fs::read(site.root.join("notes.txt")).unwrap(), b"notes.txt");
        if stopped {
            drop(a);
            disks.attach(&stick, &at);
            on_the_disk(&at, &["photo.jpg"]);
            a = restarted(&site, &mut remote);
            a.rest(&mut remote);
        } else {
            disks.attach(&stick, &at);
            on_the_disk(&at, &["photo.jpg"]);
            observed(&mut a, &mut remote);
        }
        let synced = user_files(&remote);
        assert!(synced.contains_key("Disk/photo.jpg"), "{case}: {synced:?}");
        assert!(!synced.contains_key("Disk/report.docx"), "{synced:?}");
        assert_eq!(synced["notes.txt"], tree["notes.txt"]);
        assert_eq!(held(&a), []);
        drop(a);
        a = restarted(&site, &mut remote);
        a.rest(&mut remote);
        on_the_disk(&at, &["later.txt"]);
        observed(&mut a, &mut remote);
        assert!(remote.tree().contains_key("Disk/later.txt"), "{case}");
        disks.detach(&at);
        observed(&mut a, &mut remote);
        assert_eq!(held(&a), [("Disk".to_owned(), HeldDiskKind::NotConnected)]);
        assert!(user_files(&remote).contains_key("Disk/later.txt"));
    }
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
    let image = disks.image("Disk");
    let (site, a, _image) = with_disk(&mut disks, &mut remote, "Disk", image);
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
