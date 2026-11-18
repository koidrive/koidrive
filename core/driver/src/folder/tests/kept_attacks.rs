fn kept_report(site: &Site, b: &Device, remote: &Remote) -> String {
    format!(
        "sent={:?}\nremote={:?}\nnodes={:?}\ndisk={:?}\nkept={:?}\nblocked={:?}\nview={:?}/{:?}",
        b.sent,
        remote.tree(),
        server_nodes(remote),
        entries(&site.root),
        b.folder.engine.trees.state().kept_folders,
        b.folder.engine.effects.blocked(),
        b.folder.view().state,
        b.folder.view().problem,
    )
}

fn settle_b(b: &mut Device, remote: &mut Remote) {
    b.pull(remote);
    b.run(remote);
    b.settle();
    b.run(remote);
    b.settle();
    b.run(remote);
}

#[test]
fn attack_restore_of_the_kept_folder_from_the_trash_on_another_device() {
    let ignored = [".env", "node_modules/pkg/index.js"];
    let (mut remote, site_a, mut a, site, mut b, old) =
        deleted_with_ignored_entries(&["README.md"], &ignored);
    ask(&site_a, &RestoreIntent::from_trash(old.clone(), None, None).unwrap());
    let step = a.folder.requested();
    a.take(step).unwrap();
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    let after_a = remote.tree();
    settle_b(&mut b, &mut remote);
    let report = kept_report(&site, &b, &remote);
    assert!(b.sent.is_empty(), "{report}");
    assert_eq!(remote.tree(), after_a, "{report}");
    for path in after_a.keys() {
        assert!(site.root.join(path).exists(), "{path} missing on b\n{report}");
    }
    assert_kept(&site, &b, &ignored);
}

#[test]
fn attack_a_folder_another_device_makes_at_a_kept_folders_place_in_other_case() {
    let ignored = [".env"];
    let (mut remote, site_a, mut a, site, mut b, _) =
        deleted_with_ignored_entries(&["README.md"], &ignored);
    fs::create_dir(site_a.root.join("App")).unwrap();
    a.create(&mut remote, &site_a.root.join("App/README.md"), b"cloned again");
    let after_a = remote.tree();
    settle_b(&mut b, &mut remote);
    let report = kept_report(&site, &b, &remote);
    assert!(b.sent.is_empty(), "nothing is sent\n{report}");
    assert_eq!(remote.tree(), after_a, "{report}");
    let folder = (names(&site.root).into_iter())
        .find(|name| name.eq_ignore_ascii_case("app"))
        .unwrap();
    let env = site.root.join(&folder).join(".env");
    assert_eq!(fs::read_to_string(&env).unwrap(), local_only(&env));
    assert_eq!(fs::read(site.root.join(&folder).join("README.md")).unwrap(), b"cloned again", "{report}");
}

#[test]
fn attack_a_folder_another_device_makes_at_a_kept_folders_place_in_other_normalization() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    let nfc = "caf\u{e9}";
    let nfd = "cafe\u{301}";
    fs::create_dir(site_a.root.join(nfc)).unwrap();
    a.create(&mut remote, &site_a.root.join(nfc).join("README.md"), b"synced");
    let site = Site::new(&remote, "b");
    let mut b = site.start_with(&ignore_rules());
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    b.settle();
    fs::write(site.root.join(nfc).join(".env"), b"SECRET=local").unwrap();
    b.settle();
    fs::remove_dir_all(site_a.root.join(nfc)).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    settle_b(&mut b, &mut remote);
    assert!(b.sent.is_empty());
    fs::create_dir(site_a.root.join(nfd)).unwrap();
    a.create(&mut remote, &site_a.root.join(nfd).join("README.md"), b"cloned again");
    let after_a = remote.tree();
    settle_b(&mut b, &mut remote);
    let report = kept_report(&site, &b, &remote);
    assert!(b.sent.is_empty(), "nothing is sent\n{report}");
    assert_eq!(remote.tree(), after_a, "{report}");
}

#[test]
fn attack_a_synced_file_written_before_the_scan_sees_it_survives_the_delete() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"synced");
    let site = Site::new(&remote, "b");
    let mut b = site.start_with(&ignore_rules());
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    b.settle();
    fs::write(site.root.join("app/.env"), b"SECRET=local").unwrap();
    b.settle();
    fs::remove_dir_all(site_a.root.join("app")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    fs::write(site.root.join("app/new.txt"), b"written on b").unwrap();
    b.pull(&remote);
    b.run(&mut remote);
    settle_b(&mut b, &mut remote);
    let report = kept_report(&site, &b, &remote);
    assert_eq!(fs::read(site.root.join("app/new.txt")).unwrap(), b"written on b", "{report}");
    assert_eq!(fs::read(site.root.join("app/.env")).unwrap(), b"SECRET=local", "{report}");
    let tree = remote.tree();
    assert!(tree.contains_key("app/new.txt"), "{report}");
    assert!(!tree.contains_key("app/.env"), "{report}");
    assert!(!tree.contains_key("app/README.md"), "{report}");
}

#[test]
fn attack_a_synced_file_scanned_but_unsent_survives_the_delete() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"synced");
    let site = Site::new(&remote, "b");
    let mut b = site.start_with(&ignore_rules());
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    b.settle();
    fs::write(site.root.join("app/.env"), b"SECRET=local").unwrap();
    b.settle();
    fs::remove_dir_all(site_a.root.join("app")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    fs::write(site.root.join("app/new.txt"), b"written on b").unwrap();
    b.settle();
    b.pull(&remote);
    settle_b(&mut b, &mut remote);
    let report = kept_report(&site, &b, &remote);
    assert_eq!(fs::read(site.root.join("app/new.txt")).unwrap(), b"written on b", "{report}");
    let tree = remote.tree();
    assert!(tree.contains_key("app/new.txt"), "{report}");
    assert!(!tree.contains_key("app/.env"), "{report}");
}

#[test]
fn attack_a_delete_and_a_parent_rename_in_one_pull() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir_all(site_a.root.join("proj/app")).unwrap();
    a.create(&mut remote, &site_a.root.join("proj/notes.txt"), b"synced");
    a.create(&mut remote, &site_a.root.join("proj/app/README.md"), b"synced");
    let site = Site::new(&remote, "b");
    let mut b = site.start_with(&ignore_rules());
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    b.settle();
    fs::write(site.root.join("proj/app/.env"), b"SECRET=local").unwrap();
    b.settle();
    fs::remove_dir_all(site_a.root.join("proj/app")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    fs::rename(site_a.root.join("proj"), site_a.root.join("renamed")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.pull_at_once(&remote);
    b.run(&mut remote);
    settle_b(&mut b, &mut remote);
    let report = kept_report(&site, &b, &remote);
    assert_eq!(fs::read(site.root.join("renamed/app/.env")).unwrap(), b"SECRET=local", "{report}");
    assert!(b.sent.is_empty(), "{report}");
    assert_eq!(
        remote.tree().keys().map(String::as_str).collect::<Vec<_>>(),
        ["renamed", "renamed/notes.txt"], "{report}"
    );
    let view = b.folder.view();
    assert_eq!((view.state, view.problem), (FolderState::UpToDate, None), "{report}");
}

#[test]
fn attack_a_kept_folder_deleted_here_ends_its_record_and_a_later_create_downloads() {
    let ignored = [".env"];
    let (mut remote, site_a, mut a, site, mut b, _) =
        deleted_with_ignored_entries(&["README.md"], &ignored);
    fs::remove_dir_all(site.root.join("app")).unwrap();
    b.settle();
    b.run(&mut remote);
    assert!(b.sent.is_empty(), "{:?}", b.sent);
    assert!(b.folder.engine.trees.state().kept_folders.is_empty());
    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"cloned again");
    settle_b(&mut b, &mut remote);
    let report = kept_report(&site, &b, &remote);
    assert_eq!(fs::read(site.root.join("app/README.md")).unwrap(), b"cloned again", "{report}");
    assert!(b.sent.is_empty(), "{report}");
}

#[test]
fn attack_a_kept_folder_renamed_here_then_a_create_at_the_old_name() {
    let ignored = [".env"];
    let (mut remote, site_a, mut a, site, mut b, _) =
        deleted_with_ignored_entries(&["README.md"], &ignored);
    fs::rename(site.root.join("app"), site.root.join("app-old")).unwrap();
    b.settle();
    b.run(&mut remote);
    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"cloned again");
    settle_b(&mut b, &mut remote);
    let report = kept_report(&site, &b, &remote);
    assert_eq!(fs::read(site.root.join("app/README.md")).unwrap(), b"cloned again", "{report}");
    assert!(!site.root.join("app/.env").exists(), "{report}");
    let env = site.root.join("app-old/.env");
    assert_eq!(fs::read_to_string(&env).unwrap(), local_only(&env));
    assert!(b.sent.is_empty(), "{report}");
    let view = b.folder.view();
    assert_eq!((view.state, view.problem), (FolderState::UpToDate, None), "{report}");
}

#[test]
fn attack_a_file_added_while_stopped_uploads_a_new_folder_after_the_restart() {
    let ignored = [".env", "node_modules/pkg/index.js"];
    let (mut remote, _, _, site, b, old) = deleted_with_ignored_entries(&["README.md"], &ignored);
    drop(b);
    fs::write(site.root.join("app/todo.txt"), b"written while stopped").unwrap();
    let mut b = site.start_with(&ignore_rules());
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    let report = kept_report(&site, &b, &remote);
    let tree = remote.tree();
    assert_eq!(tree.keys().map(String::as_str).collect::<Vec<_>>(), ["app", "app/todo.txt"], "{report}");
    assert_ne!(tree["app"], old);
    assert_kept(&site, &b, &ignored);
}

#[test]
fn attack_a_synced_file_moved_into_a_kept_folder() {
    let ignored = [".env"];
    let (mut remote, site_a, mut a, site, mut b, old) =
        deleted_with_ignored_entries(&["README.md"], &ignored);
    a.create(&mut remote, &site_a.root.join("keep.txt"), b"kept elsewhere");
    settle_b(&mut b, &mut remote);
    fs::rename(site.root.join("keep.txt"), site.root.join("app/keep.txt")).unwrap();
    b.settle();
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    let report = kept_report(&site, &b, &remote);
    let tree = remote.tree();
    assert_eq!(tree.keys().map(String::as_str).collect::<Vec<_>>(), ["app", "app/keep.txt"], "{report}");
    assert_ne!(tree["app"], old);
    assert!(!tree.contains_key("app/.env"));
    assert_eq!(fs::read(site.root.join("app/keep.txt")).unwrap(), b"kept elsewhere");
}

#[test]
fn attack_a_kept_folder_whose_rules_file_was_synced_uploads_nothing() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/.koiignore"), b".env\nnode_modules/\n");
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"synced");
    let site = Site::new(&remote, "b");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    b.settle();
    fs::write(site.root.join("app/.env"), b"SECRET=local").unwrap();
    fs::create_dir_all(site.root.join("app/node_modules/pkg")).unwrap();
    fs::write(site.root.join("app/node_modules/pkg/index.js"), b"built").unwrap();
    b.settle();
    b.run(&mut remote);
    assert!(!remote.tree().contains_key("app/.env"), "{:?}", remote.tree());
    let before = b.sent.len();

    fs::remove_dir_all(site_a.root.join("app")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    settle_b(&mut b, &mut remote);
    let report = kept_report(&site, &b, &remote);
    let tree = remote.tree();
    assert!(!tree.keys().any(|k| k.contains(".env") || k.contains("node_modules")), "secrets uploaded\n{report}");
    assert_eq!(b.sent.len(), before, "nothing is sent\n{report}");
    assert_eq!(fs::read(site.root.join("app/.env")).unwrap(), b"SECRET=local", "{report}");
}

fn rules_world(koi_dir: &str, koi: &[u8], synced: &[&str], local: &[&str]) -> (Remote, Site, Device, Site, Device) {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir_all(site_a.root.join(koi_dir)).unwrap();
    a.create(&mut remote, &site_a.root.join(koi_dir).join(".koiignore"), koi);
    for path in synced {
        let path = site_a.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        a.create(&mut remote, &path, b"synced");
    }
    let site = Site::new(&remote, "b");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    eventually("b holds every synced path", || {
        b.run(&mut remote);
        b.settle();
        synced.iter().all(|path| site.root.join(path).exists())
    });
    for path in local {
        let path = site.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"SECRET=local").unwrap();
    }
    b.settle();
    b.run(&mut remote);
    assert!(remote.tree().keys().all(|k| !local.contains(&k.as_str())), "{:?}", remote.tree());
    (remote, site_a, a, site, b)
}

fn delete_on_a(site_a: &Site, a: &mut Device, remote: &mut Remote, path: &str) {
    fs::remove_dir_all(site_a.root.join(path)).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(remote);
}

fn no_secrets(remote: &Remote, report: &str) {
    assert!(
        !remote.tree().keys().any(|k| k.ends_with(".env") || k.contains("node_modules")),
        "secrets on the server\n{report}"
    );
}

const KOI: &[u8] = b".env\nnode_modules/\n";
const LOCAL: [&str; 2] = ["app/.env", "app/node_modules/pkg/index.js"];

#[test]
fn attack_k_restart_with_a_kept_rules_file() {
    let (mut remote, site_a, mut a, site, mut b) = rules_world("app", KOI, &["app/README.md"], &LOCAL);
    delete_on_a(&site_a, &mut a, &mut remote, "app");
    settle_b(&mut b, &mut remote);
    let sent = b.sent.len();
    drop(b);
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    settle_b(&mut b, &mut remote);
    let report = kept_report(&site, &b, &remote);
    no_secrets(&remote, &report);
    assert!(remote.tree().is_empty(), "{report}");
    assert_eq!(b.sent.len(), 0, "{sent} {report}");
    assert!(site.root.join("app/.koiignore").exists(), "{report}");
    assert_eq!(b.folder.view().problem, None, "{report}");
}

#[test]
fn attack_k_file_added_while_stopped_to_a_kept_folder_with_rules() {
    let (mut remote, site_a, mut a, site, mut b) = rules_world("app", KOI, &["app/README.md"], &LOCAL);
    delete_on_a(&site_a, &mut a, &mut remote, "app");
    settle_b(&mut b, &mut remote);
    drop(b);
    fs::write(site.root.join("app/todo.txt"), b"todo").unwrap();
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    settle_b(&mut b, &mut remote);
    let report = kept_report(&site, &b, &remote);
    no_secrets(&remote, &report);
    let keys: Vec<String> = remote.tree().keys().cloned().collect();
    assert_eq!(keys, ["app", "app/.koiignore", "app/todo.txt"], "{report}");
}

#[test]
fn attack_k_reclone_with_different_rules() {
    let (mut remote, site_a, mut a, site, mut b) = rules_world("app", KOI, &["app/README.md"], &LOCAL);
    delete_on_a(&site_a, &mut a, &mut remote, "app");
    settle_b(&mut b, &mut remote);
    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/.koiignore"), b".env\nnode_modules/\n*.log\n");
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"cloned again");
    let after_a = remote.tree();
    settle_b(&mut b, &mut remote);
    let report = kept_report(&site, &b, &remote);
    no_secrets(&remote, &report);
    assert_eq!(fs::read(site.root.join("app/.env")).unwrap(), b"SECRET=local", "{report}");
    assert_eq!(fs::read(site.root.join("app/README.md")).unwrap(), b"cloned again", "{report}");
    assert_eq!(remote.tree(), after_a, "{report}");
    assert_eq!(b.folder.view().problem, None, "{report}");
}

#[test]
fn attack_k_rules_in_the_parent_of_the_deleted_tree() {
    let (mut remote, site_a, mut a, site, mut b) = rules_world(
        "proj",
        b".env\nnode_modules/\n",
        &["proj/README.md", "proj/app/README.md"],
        &["proj/app/.env", "proj/app/node_modules/pkg/index.js"],
    );
    delete_on_a(&site_a, &mut a, &mut remote, "proj");
    settle_b(&mut b, &mut remote);
    let report = kept_report(&site, &b, &remote);
    no_secrets(&remote, &report);
    assert!(remote.tree().is_empty(), "{report}");
    assert!(b.sent.is_empty(), "{report}");
    assert_eq!(fs::read(site.root.join("proj/app/.env")).unwrap(), b"SECRET=local", "{report}");
    assert_eq!(b.folder.view().problem, None, "{report}");
}

#[test]
fn attack_k_remote_rules_edit_and_delete_in_one_pull() {
    let (mut remote, site_a, mut a, site, mut b) = rules_world("app", KOI, &["app/README.md"], &LOCAL);
    fs::write(site_a.root.join("app/.koiignore"), b".env\n").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    delete_on_a(&site_a, &mut a, &mut remote, "app");
    b.pull_at_once(&remote);
    b.run(&mut remote);
    settle_b(&mut b, &mut remote);
    let report = kept_report(&site, &b, &remote);
    assert!(!remote.tree().keys().any(|k| k.ends_with(".env")), "{report}");
    assert_eq!(fs::read(site.root.join("app/.env")).unwrap(), b"SECRET=local", "{report}");
}

#[test]
fn attack_k_local_rules_edit_racing_the_delete() {
    let (mut remote, site_a, mut a, site, mut b) = rules_world("app", KOI, &["app/README.md"], &LOCAL);
    delete_on_a(&site_a, &mut a, &mut remote, "app");
    fs::write(site.root.join("app/.koiignore"), b".env\nnode_modules/\n*.log\n").unwrap();
    b.settle();
    b.pull(&remote);
    b.run(&mut remote);
    settle_b(&mut b, &mut remote);
    let report = kept_report(&site, &b, &remote);
    no_secrets(&remote, &report);
    let on_disk = fs::read(site.root.join("app/.koiignore")).ok();
    let on_server = remote.tree().contains_key("app/.koiignore");
    assert!(
        on_disk.as_deref() == Some(&b".env\nnode_modules/\n*.log\n"[..]) || on_server,
        "the edit is lost\n{report}"
    );
}

#[test]
fn attack_k_unscanned_local_rules_edit_racing_the_delete() {
    let (mut remote, site_a, mut a, site, mut b) = rules_world("app", KOI, &["app/README.md"], &LOCAL);
    delete_on_a(&site_a, &mut a, &mut remote, "app");
    fs::write(site.root.join("app/.koiignore"), b".env\nnode_modules/\n*.log\n").unwrap();
    b.pull(&remote);
    b.run(&mut remote);
    settle_b(&mut b, &mut remote);
    let report = kept_report(&site, &b, &remote);
    no_secrets(&remote, &report);
    let on_disk = fs::read(site.root.join("app/.koiignore")).ok();
    assert_eq!(on_disk.as_deref(), Some(&b".env\nnode_modules/\n*.log\n"[..]), "edit not in place\n{report}");
}

#[test]
fn attack_k_rename_a_kept_folder_with_rules() {
    let (mut remote, site_a, mut a, site, mut b) = rules_world("app", KOI, &["app/README.md"], &LOCAL);
    delete_on_a(&site_a, &mut a, &mut remote, "app");
    settle_b(&mut b, &mut remote);
    fs::rename(site.root.join("app"), site.root.join("app-old")).unwrap();
    b.settle();
    b.run(&mut remote);
    let report = kept_report(&site, &b, &remote);
    assert!(b.sent.is_empty(), "{report}");
    assert!(remote.tree().is_empty(), "{report}");
}

#[test]
fn attack_k_reclone_without_rules() {
    let (mut remote, site_a, mut a, site, mut b) = rules_world("app", KOI, &["app/README.md"], &LOCAL);
    delete_on_a(&site_a, &mut a, &mut remote, "app");
    settle_b(&mut b, &mut remote);
    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"cloned again");
    settle_b(&mut b, &mut remote);
    let report = kept_report(&site, &b, &remote);
    no_secrets(&remote, &report);
    assert_eq!(fs::read(site.root.join("app/.env")).unwrap(), b"SECRET=local", "{report}");
}

#[test]
fn attack_k_reclone_without_rules_after_restart() {
    let (mut remote, site_a, mut a, site, mut b) = rules_world("app", KOI, &["app/README.md"], &LOCAL);
    delete_on_a(&site_a, &mut a, &mut remote, "app");
    settle_b(&mut b, &mut remote);
    drop(b);
    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"cloned again");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    settle_b(&mut b, &mut remote);
    let report = kept_report(&site, &b, &remote);
    no_secrets(&remote, &report);
}

fn taken_over_without_rules(ignored: &[&str]) -> (Remote, Site, Device, Site, Device) {
    let (mut remote, site_a, mut a, site, mut b, _) = deleted_with_rules_files(
        &[(".koiignore", ".env\nnode_modules/\n*.key\n")],
        &["README.md"],
        ignored,
    );
    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"cloned again");
    for _ in 0..3 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }
    assert!(!remote.tree().contains_key("app/.env"), "{:?}", remote.tree());
    (remote, site_a, a, site, b)
}

fn r4_report(site: &Site, b: &Device, remote: &Remote) -> String {
    format!(
        "remote={:?}\ndisk={:?}\nkept_entries={:?}\nview={:?}/{:?}",
        remote.tree(),
        entries(&site.root),
        b.folder.engine.trees.kept_entry_paths(),
        b.folder.view().state,
        b.folder.view().problem,
    )
}

#[test]
fn attack_r4_atomic_save_of_a_kept_env_after_a_takeover() {
    let (mut remote, _sa, _a, site, mut b) = taken_over_without_rules(&[".env", "node_modules/pkg/index.js"]);
    let env = site.root.join("app/.env");
    let tmp = site.root.join("app/.env.swp~");
    fs::write(&tmp, b"SECRET=edited").unwrap();
    fs::rename(&tmp, &env).unwrap();
    for _ in 0..3 {
        b.settle();
        b.run(&mut remote);
    }
    let report = r4_report(&site, &b, &remote);
    assert!(!remote.tree().contains_key("app/.env"), "an editor's save uploads the secret\n{report}");
    assert_eq!(fs::read(&env).unwrap(), b"SECRET=edited");
}

#[test]
fn attack_r4_in_place_edit_of_a_kept_env_after_a_takeover() {
    let (mut remote, _sa, _a, site, mut b) = taken_over_without_rules(&[".env"]);
    fs::write(site.root.join("app/.env"), b"SECRET=edited in place").unwrap();
    for _ in 0..3 {
        b.settle();
        b.run(&mut remote);
    }
    let report = r4_report(&site, &b, &remote);
    assert!(!remote.tree().contains_key("app/.env"), "{report}");
}

#[test]
fn attack_r4_nfd_named_kept_entry_after_a_takeover() {
    let name = "cafe\u{301}.key";
    let (remote, _sa, _a, site, b) = taken_over_without_rules(&[".env", name]);
    let report = r4_report(&site, &b, &remote);
    assert!(!remote.tree().keys().any(|k| k.ends_with(".key")), "{report}");
}

#[test]
fn attack_r4_new_file_inside_kept_node_modules_after_a_takeover() {
    let (mut remote, _sa, _a, site, mut b) = taken_over_without_rules(&[".env", "node_modules/pkg/index.js"]);
    fs::create_dir_all(site.root.join("app/node_modules/other")).unwrap();
    fs::write(site.root.join("app/node_modules/other/x.js"), b"built later").unwrap();
    for _ in 0..3 {
        b.settle();
        b.run(&mut remote);
    }
    let report = r4_report(&site, &b, &remote);
    assert!(!remote.tree().keys().any(|k| k.contains("node_modules")), "{report}");
}

#[test]
fn attack_r4_incoming_env_over_a_kept_env() {
    let (mut remote, site_a, mut a, site, mut b, _) = deleted_with_rules_files(
        &[(".koiignore", ".env\nnode_modules/\n")],
        &["README.md"],
        &[".env"],
    );
    let before = fs::read(site.root.join("app/.env")).unwrap();
    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/.env"), b"SECRET=remote");
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"cloned again");
    for _ in 0..3 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }
    let report = r4_report(&site, &b, &remote);
    let mine = (fs::read_dir(site.root.join("app")).unwrap())
        .filter_map(|e| fs::read(e.unwrap().path()).ok())
        .any(|content| content == before);
    assert!(mine, "the local .env content is gone from the folder\n{report}");
    assert_eq!(remote.tree().get("app/.env").map(|_| ()), Some(()), "{report}");
    assert!(remote.tree().keys().filter(|k| k.contains(".env")).count() == 1, "local .env uploaded\n{report}");
}

#[test]
fn attack_r4_restart_then_atomic_save_after_a_takeover() {
    let (mut remote, _sa, _a, site, b) = taken_over_without_rules(&[".env"]);
    drop(b);
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    for _ in 0..2 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }
    let env = site.root.join("app/.env");
    let tmp = site.root.join("app/.env.swp~");
    fs::write(&tmp, b"SECRET=edited").unwrap();
    fs::rename(&tmp, &env).unwrap();
    for _ in 0..3 {
        b.settle();
        b.run(&mut remote);
    }
    let report = r4_report(&site, &b, &remote);
    assert!(!remote.tree().contains_key("app/.env"), "{report}");
}

#[test]
fn attack_r5_backup_by_rename_save_of_a_kept_env_after_a_takeover() {
    let (mut remote, _sa, _a, site, mut b) = taken_over_without_rules(&[".env"]);
    let env = site.root.join("app/.env");
    fs::rename(&env, site.root.join("app/.env~")).unwrap();
    fs::write(&env, b"SECRET=edited").unwrap();
    for _ in 0..3 {
        b.settle();
        b.run(&mut remote);
    }
    let report = r4_report(&site, &b, &remote);
    assert!(!remote.tree().keys().any(|k| k.contains(".env")), "an Emacs-style save uploads the secret\n{report}");
}

fn r6_settle(b: &mut Device, remote: &mut Remote) {
    for _ in 0..3 {
        b.settle();
        b.run(remote);
    }
}

#[test]
fn attack_r6_backup_by_rename_while_stopped() {
    let (mut remote, _sa, _a, site, b) = taken_over_without_rules(&[".env"]);
    drop(b);
    let env = site.root.join("app/.env");
    fs::rename(&env, site.root.join("app/.env~")).unwrap();
    fs::write(&env, b"SECRET=edited").unwrap();
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.pull(&remote);
    r6_settle(&mut b, &mut remote);
    let report = r4_report(&site, &b, &remote);
    assert!(!remote.tree().keys().any(|k| k.contains(".env")), "{report}");
}

#[test]
fn attack_r6_two_backup_cycles() {
    let (mut remote, _sa, _a, site, mut b) = taken_over_without_rules(&[".env"]);
    let env = site.root.join("app/.env");
    for round in 0..2 {
        fs::rename(&env, site.root.join("app/.env~")).unwrap();
        fs::write(&env, format!("SECRET=edit {round}")).unwrap();
        r6_settle(&mut b, &mut remote);
    }
    let report = r4_report(&site, &b, &remote);
    assert!(!remote.tree().keys().any(|k| k.contains(".env")), "{report}");
}

#[test]
fn attack_r6_bound_entry_moved_out_of_the_kept_folder() {
    let (mut remote, site_a, mut a, site, mut b) = taken_over_without_rules(&[".env"]);
    fs::create_dir(site_a.root.join("other")).unwrap();
    a.create(&mut remote, &site_a.root.join("other/notes.txt"), b"synced");
    b.pull(&remote);
    r6_settle(&mut b, &mut remote);
    fs::rename(site.root.join("app/.env"), site.root.join("other/.env")).unwrap();
    r6_settle(&mut b, &mut remote);
    let report = r4_report(&site, &b, &remote);
    assert!(!remote.tree().keys().any(|k| k.contains(".env")), "{report}");
}

#[test]
fn attack_r6_renamed_kept_node_modules_then_a_new_file_below() {
    let (mut remote, _sa, _a, site, mut b) = taken_over_without_rules(&[".env", "node_modules/pkg/index.js"]);
    fs::rename(site.root.join("app/node_modules"), site.root.join("app/deps")).unwrap();
    r6_settle(&mut b, &mut remote);
    fs::write(site.root.join("app/deps/pkg/new.js"), b"built later").unwrap();
    r6_settle(&mut b, &mut remote);
    let report = r4_report(&site, &b, &remote);
    assert!(!remote.tree().keys().any(|k| k.contains("deps") || k.contains("node_modules")), "{report}");
}

fn r7_secret_free(remote: &Remote, report: &str) {
    assert!(
        !remote.tree().keys().any(|k| k.contains("node_modules") || k.contains("new.js") || (k.contains(".env") && k != ".env")),
        "secrets on the server\n{report}"
    );
}

fn r7_moved_out(to: &str) -> (Remote, Site, Device, Site, Device) {
    let (mut remote, site_a, mut a, site, mut b) = taken_over_without_rules(&[".env", "node_modules/pkg/index.js"]);
    fs::create_dir(site_a.root.join("other")).unwrap();
    a.create(&mut remote, &site_a.root.join("other/notes.txt"), b"synced");
    b.pull(&remote);
    r6_settle(&mut b, &mut remote);
    assert!(site.root.join("other/notes.txt").exists());
    let dest = site.root.join(to);
    fs::rename(site.root.join("app/.env"), dest.join(".env")).unwrap();
    fs::rename(site.root.join("app/node_modules"), dest.join("node_modules")).unwrap();
    r6_settle(&mut b, &mut remote);
    let report = r4_report(&site, &b, &remote);
    r7_secret_free(&remote, &report);
    (remote, site_a, a, site, b)
}

fn r7_edit_and_check(site: &Site, b: &mut Device, remote: &mut Remote, env: &str) {
    fs::write(site.root.join(env), b"SECRET=edited").unwrap();
    fs::write(site.root.join(env).parent().unwrap().join("node_modules/pkg/new.js"), b"SECRET built").unwrap();
    r6_settle(b, remote);
    let report = r4_report(site, b, remote);
    r7_secret_free(remote, &report);
    assert_eq!(fs::read(site.root.join(env)).unwrap(), b"SECRET=edited", "{report}");
}

#[test]
fn attack_r7_moved_out_then_both_folders_deleted_remotely() {
    let (mut remote, site_a, mut a, site, mut b) = r7_moved_out("other");
    delete_on_a(&site_a, &mut a, &mut remote, "app");
    settle_b(&mut b, &mut remote);
    delete_on_a(&site_a, &mut a, &mut remote, "other");
    settle_b(&mut b, &mut remote);
    assert!(site.root.join("other/.env").exists(), "{}", r4_report(&site, &b, &remote));
    r7_edit_and_check(&site, &mut b, &mut remote, "other/.env");
}

#[test]
fn attack_r7_moved_out_then_new_home_deleted_first() {
    let (mut remote, site_a, mut a, site, mut b) = r7_moved_out("other");
    delete_on_a(&site_a, &mut a, &mut remote, "other");
    settle_b(&mut b, &mut remote);
    delete_on_a(&site_a, &mut a, &mut remote, "app");
    settle_b(&mut b, &mut remote);
    assert!(site.root.join("other/.env").exists(), "{}", r4_report(&site, &b, &remote));
    r7_edit_and_check(&site, &mut b, &mut remote, "other/.env");
}

#[test]
fn attack_r7_moved_out_both_deleted_then_new_home_taken_over() {
    let (mut remote, site_a, mut a, site, mut b) = r7_moved_out("other");
    delete_on_a(&site_a, &mut a, &mut remote, "app");
    settle_b(&mut b, &mut remote);
    delete_on_a(&site_a, &mut a, &mut remote, "other");
    settle_b(&mut b, &mut remote);
    fs::create_dir(site_a.root.join("other")).unwrap();
    a.create(&mut remote, &site_a.root.join("other/notes.txt"), b"made again");
    for _ in 0..3 {
        settle_b(&mut b, &mut remote);
    }
    r7_edit_and_check(&site, &mut b, &mut remote, "other/.env");
    assert_eq!(fs::read(site.root.join("other/notes.txt")).unwrap(), b"made again");
}

#[test]
fn attack_r7_moved_out_restart_then_app_deleted() {
    let (mut remote, site_a, mut a, site, b) = r7_moved_out("other");
    drop(b);
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    settle_b(&mut b, &mut remote);
    delete_on_a(&site_a, &mut a, &mut remote, "app");
    settle_b(&mut b, &mut remote);
    r7_edit_and_check(&site, &mut b, &mut remote, "other/.env");
}

#[test]
fn attack_r7_moved_out_app_deleted_restart_then_atomic_save() {
    let (mut remote, site_a, mut a, site, mut b) = r7_moved_out("other");
    delete_on_a(&site_a, &mut a, &mut remote, "app");
    settle_b(&mut b, &mut remote);
    drop(b);
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    settle_b(&mut b, &mut remote);
    let tmp = site.root.join("other/.env.swp~");
    fs::write(&tmp, b"SECRET=saved").unwrap();
    fs::rename(&tmp, site.root.join("other/.env")).unwrap();
    r7_edit_and_check(&site, &mut b, &mut remote, "other/.env");
}

#[test]
fn attack_r7_moved_out_app_deleted_then_moved_back_into_a_new_app() {
    let (mut remote, site_a, mut a, site, mut b) = r7_moved_out("other");
    delete_on_a(&site_a, &mut a, &mut remote, "app");
    settle_b(&mut b, &mut remote);
    assert!(!site.root.join("app").exists(), "{}", r4_report(&site, &b, &remote));
    fs::create_dir(site.root.join("app")).unwrap();
    fs::write(site.root.join("app/main.rs"), b"fn main() {}").unwrap();
    fs::rename(site.root.join("other/.env"), site.root.join("app/.env")).unwrap();
    fs::rename(site.root.join("other/node_modules"), site.root.join("app/node_modules")).unwrap();
    r6_settle(&mut b, &mut remote);
    r7_edit_and_check(&site, &mut b, &mut remote, "app/.env");
    assert!(remote.tree().contains_key("app/main.rs"), "{}", r4_report(&site, &b, &remote));
}

#[test]
fn attack_r7_moved_out_new_home_renamed_remotely_then_both_deleted() {
    let (mut remote, site_a, mut a, site, mut b) = r7_moved_out("other");
    fs::rename(site_a.root.join("other"), site_a.root.join("other2")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    settle_b(&mut b, &mut remote);
    assert!(site.root.join("other2/.env").exists(), "{}", r4_report(&site, &b, &remote));
    delete_on_a(&site_a, &mut a, &mut remote, "app");
    settle_b(&mut b, &mut remote);
    delete_on_a(&site_a, &mut a, &mut remote, "other2");
    settle_b(&mut b, &mut remote);
    r7_edit_and_check(&site, &mut b, &mut remote, "other2/.env");
}

#[test]
fn attack_r7_moved_twice_then_middle_and_app_deleted() {
    let (mut remote, site_a, mut a, site, mut b) = r7_moved_out("other");
    fs::create_dir(site.root.join("deeper")).unwrap();
    fs::write(site.root.join("deeper/keep.txt"), b"synced too").unwrap();
    fs::rename(site.root.join("other/.env"), site.root.join("deeper/.env")).unwrap();
    fs::rename(site.root.join("other/node_modules"), site.root.join("deeper/node_modules")).unwrap();
    r6_settle(&mut b, &mut remote);
    settle_b(&mut b, &mut remote);
    for _ in 0..2 { a.pull(&remote); a.run(&mut remote); a.settle(); }
    delete_on_a(&site_a, &mut a, &mut remote, "other");
    settle_b(&mut b, &mut remote);
    delete_on_a(&site_a, &mut a, &mut remote, "app");
    settle_b(&mut b, &mut remote);
    r7_edit_and_check(&site, &mut b, &mut remote, "deeper/.env");
    delete_on_a(&site_a, &mut a, &mut remote, "deeper");
    settle_b(&mut b, &mut remote);
    r7_edit_and_check(&site, &mut b, &mut remote, "deeper/.env");
}

#[test]
fn attack_r7_moved_out_into_a_subfolder_of_app_then_app_deleted() {
    let (mut remote, site_a, mut a, site, mut b) = taken_over_without_rules(&[".env", "node_modules/pkg/index.js"]);
    fs::create_dir(site.root.join("app/sub")).unwrap();
    fs::write(site.root.join("app/sub/lib.rs"), b"synced").unwrap();
    fs::rename(site.root.join("app/.env"), site.root.join("app/sub/.env")).unwrap();
    fs::rename(site.root.join("app/node_modules"), site.root.join("app/sub/node_modules")).unwrap();
    r6_settle(&mut b, &mut remote);
    settle_b(&mut b, &mut remote);
    a.pull(&remote);
    a.run(&mut remote);
    a.settle();
    delete_on_a(&site_a, &mut a, &mut remote, "app");
    settle_b(&mut b, &mut remote);
    r7_edit_and_check(&site, &mut b, &mut remote, "app/sub/.env");
    fs::rename(site.root.join("app/sub"), site.root.join("sub")).unwrap();
    r6_settle(&mut b, &mut remote);
    r7_edit_and_check(&site, &mut b, &mut remote, "sub/.env");
}

#[test]
fn attack_r7_moved_to_root_then_another_device_makes_env_at_root() {
    let (mut remote, site_a, mut a, site, mut b) = taken_over_without_rules(&[".env"]);
    fs::rename(site.root.join("app/.env"), site.root.join(".env")).unwrap();
    r6_settle(&mut b, &mut remote);
    delete_on_a(&site_a, &mut a, &mut remote, "app");
    settle_b(&mut b, &mut remote);
    a.create(&mut remote, &site_a.root.join(".env"), b"PUBLIC=from a");
    for _ in 0..2 {
        settle_b(&mut b, &mut remote);
    }
    for _ in 0..2 { a.pull(&remote); a.run(&mut remote); a.settle(); }
    let report = r4_report(&site, &b, &remote);
    assert_eq!(fs::read(site_a.root.join(".env")).unwrap(), b"PUBLIC=from a", "{report}");
    assert!(!entries(&site_a.root).iter().any(|(k, _)| k.contains(".env") && k != ".env"), "{report}");
    r7_secret_free(&remote, &report);
    let kept = fs::read_dir(&site.root).unwrap().filter_map(|e| fs::read(e.unwrap().path()).ok()).any(|bytes| bytes.starts_with(b"app/.env") || String::from_utf8_lossy(&bytes).contains("local"));
    assert!(kept, "the kept secret is gone from disk\n{report}");
}

#[test]
fn attack_r7c_app_deleted_and_taken_over_twice() {
    let (mut remote, site_a, mut a, site, mut b) = taken_over_without_rules(&[".env"]);
    a.pull(&remote); a.run(&mut remote); a.settle();
    delete_on_a(&site_a, &mut a, &mut remote, "app");
    settle_b(&mut b, &mut remote);
    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"third");
    for _ in 0..3 { settle_b(&mut b, &mut remote); }
    assert!(site.root.join("app/.env").exists() && site.root.join("app/README.md").exists());
    no_secrets(&remote, "");
}

#[test]
fn attack_r7c_moved_out_new_home_deleted_and_taken_over_app_alive() {
    let (mut remote, site_a, mut a, site, mut b) = taken_over_without_rules(&[".env"]);
    fs::create_dir(site_a.root.join("other")).unwrap();
    a.create(&mut remote, &site_a.root.join("other/notes.txt"), b"synced");
    b.pull(&remote);
    r6_settle(&mut b, &mut remote);
    fs::rename(site.root.join("app/.env"), site.root.join("other/.env")).unwrap();
    r6_settle(&mut b, &mut remote);
    delete_on_a(&site_a, &mut a, &mut remote, "other");
    settle_b(&mut b, &mut remote);
    fs::create_dir(site_a.root.join("other")).unwrap();
    a.create(&mut remote, &site_a.root.join("other/notes.txt"), b"made again");
    for _ in 0..3 { settle_b(&mut b, &mut remote); }
    assert!(site.root.join("other/.env").exists() && site.root.join("other/notes.txt").exists());
    no_secrets(&remote, "");
}

#[test]
fn attack_r7c_moved_out_env_only_both_deleted_then_taken_over() {
    let (mut remote, site_a, mut a, site, mut b) = taken_over_without_rules(&[".env"]);
    fs::create_dir(site_a.root.join("other")).unwrap();
    a.create(&mut remote, &site_a.root.join("other/notes.txt"), b"synced");
    b.pull(&remote);
    r6_settle(&mut b, &mut remote);
    fs::rename(site.root.join("app/.env"), site.root.join("other/.env")).unwrap();
    r6_settle(&mut b, &mut remote);
    delete_on_a(&site_a, &mut a, &mut remote, "app");
    settle_b(&mut b, &mut remote);
    delete_on_a(&site_a, &mut a, &mut remote, "other");
    settle_b(&mut b, &mut remote);
    fs::create_dir(site_a.root.join("other")).unwrap();
    a.create(&mut remote, &site_a.root.join("other/notes.txt"), b"made again");
    for _ in 0..3 { settle_b(&mut b, &mut remote); }
    assert!(site.root.join("other/.env").exists() && site.root.join("other/notes.txt").exists());
    no_secrets(&remote, "");
}

#[test]
fn probe_a_restore_of_the_deleted_folder_brings_its_files_back_into_the_kept_folder() {
    let ignored = [".env", "node_modules/pkg/index.js"];
    let (mut remote, site_a, mut a, site, mut b, old) =
        deleted_with_ignored_entries(&["README.md"], &ignored);

    let id = ask(&site_a, &RestoreIntent::from_trash(old.clone(), None, None).unwrap());
    let step = a.folder.requested();
    a.take(step).unwrap();
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(ended(&site_a), [(id, Outcome::Restored)]);
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    b.settle();

    assert_eq!(fs::read(site.root.join("app/README.md")).unwrap(), b"synced");
    assert_eq!(local_named(&b, "app"), old);
    assert_kept(&site, &b, &ignored);
}

#[test]
fn probe_removing_the_kept_entries_leaves_what() {
    let ignored = [".env"];
    let (mut remote, _, _, site, mut b, _) = deleted_with_ignored_entries(&["README.md"], &ignored);
    fs::remove_file(site.root.join("app/.env")).unwrap();
    b.settle();
    b.run(&mut remote);
    b.settle();
    assert_eq!(b.folder.view().problem, None);
    assert!(!remote.tree().contains_key("app"), "{:?}", remote.tree());
}

fn probe_rules_setup(remote: &mut Remote) -> (Site, Device, Site, Device) {
    let (site_a, mut a) = linked(remote, "a");
    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(remote, &site_a.root.join("app/.koiignore"), b".env\n");
    a.create(remote, &site_a.root.join("app/README.md"), b"synced");
    let site = Site::new(remote, "b");
    let mut b = site.start();
    b.connect(remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(remote);
    b.settle();
    fs::write(site.root.join("app/.env"), b"SECRET=local").unwrap();
    b.settle();
    b.run(remote);
    assert!(!remote.tree().contains_key("app/.env"));
    (site_a, a, site, b)
}

#[test]
fn probe_rules_file_with_an_unsent_edit_beside_it_never_uploads_env() {
    let mut remote = Remote::new();
    let (site_a, mut a, site, mut b) = probe_rules_setup(&mut remote);
    fs::write(site.root.join("app/README.md"), b"edited on b").unwrap();
    b.settle();
    fs::remove_dir_all(site_a.root.join("app")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    for _ in 0..4 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }
    assert!(!remote.tree().contains_key("app/.env"), "{:?}", remote.tree());
    assert_eq!(fs::read(site.root.join("app/.env")).unwrap(), b"SECRET=local");
}

#[test]
fn probe_rules_file_with_an_unobserved_new_file_never_uploads_env() {
    let mut remote = Remote::new();
    let (site_a, mut a, site, mut b) = probe_rules_setup(&mut remote);
    fs::remove_dir_all(site_a.root.join("app")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.pull(&remote);
    fs::write(site.root.join("app/new.txt"), b"not seen yet").unwrap();
    for _ in 0..4 {
        b.run(&mut remote);
        b.settle();
        b.pull(&remote);
    }
    assert!(!remote.tree().contains_key("app/.env"), "{:?}", remote.tree());
    assert_eq!(fs::read(site.root.join("app/.env")).unwrap(), b"SECRET=local");
}

#[test]
fn probe_takeover_by_a_folder_without_rules_never_uploads_env() {
    let (mut remote, site_a, mut a, _site, mut b, _) =
        deleted_with_rules_files(&[(".koiignore", ".env\n")], &["README.md"], &[".env"]);
    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"cloned again");
    for _ in 0..4 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }
    assert!(!remote.tree().contains_key("app/.env"), "{:?}", remote.tree());
}

fn probe_taken_over_without_rules() -> (Remote, Site, Device, Site, Device) {
    let (mut remote, site_a, mut a, site, mut b, _) =
        deleted_with_rules_files(&[(".koiignore", ".env\n")], &["README.md"], &[".env"]);
    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"cloned again");
    for _ in 0..4 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }
    assert!(!remote.tree().contains_key("app/.env"), "{:?}", remote.tree());
    (remote, site_a, a, site, b)
}

#[test]
fn probe_an_atomic_save_of_a_kept_env_after_a_takeover_never_uploads_it() {
    let (mut remote, _, _, site, mut b) = probe_taken_over_without_rules();
    let tmp = site.root.join("app/.env.swp");
    fs::write(&tmp, b"SECRET=rotated").unwrap();
    fs::rename(&tmp, site.root.join("app/.env")).unwrap();
    for _ in 0..3 {
        b.settle();
        b.run(&mut remote);
    }
    assert!(!remote.tree().contains_key("app/.env"), "{:?}", remote.tree());
}

#[test]
fn probe_a_file_written_just_before_a_takeover_syncs() {
    let (mut remote, site_a, mut a, site, mut b, _) =
        deleted_with_rules_files(&[(".koiignore", ".env\n")], &["README.md"], &[".env"]);
    fs::create_dir(site_a.root.join("app")).unwrap();
    a.create(&mut remote, &site_a.root.join("app/README.md"), b"cloned again");
    b.pull(&remote);
    fs::write(site.root.join("app/todo.txt"), b"written as the takeover lands").unwrap();
    for _ in 0..4 {
        b.run(&mut remote);
        b.settle();
        b.pull(&remote);
    }
    assert!(remote.tree().contains_key("app/todo.txt"), "{:?}", remote.tree());
    assert!(!remote.tree().contains_key("app/.env"), "{:?}", remote.tree());
}

fn probe_r6_settle(b: &mut Device, remote: &mut Remote) {
    for _ in 0..3 {
        b.settle();
        b.run(remote);
    }
}

#[test]
fn probe_r6_move_env_to_root_and_back() {
    let (mut remote, _, _, site, mut b) = probe_taken_over_without_rules();
    fs::rename(site.root.join("app/.env"), site.root.join(".env")).unwrap();
    probe_r6_settle(&mut b, &mut remote);
    assert!(!remote.tree().contains_key(".env"), "{:?}", remote.tree());
    fs::write(site.root.join(".env"), b"SECRET=edited at root").unwrap();
    probe_r6_settle(&mut b, &mut remote);
    assert!(!remote.tree().contains_key(".env"), "{:?}", remote.tree());
    fs::rename(site.root.join(".env"), site.root.join("app/.env")).unwrap();
    probe_r6_settle(&mut b, &mut remote);
    assert!(!remote.tree().contains_key("app/.env"), "{:?}", remote.tree());
}

#[test]
fn probe_r6_move_env_out_then_delete_the_folder_here() {
    let (mut remote, _, _, site, mut b) = probe_taken_over_without_rules();
    fs::rename(site.root.join("app/.env"), site.root.join(".env")).unwrap();
    probe_r6_settle(&mut b, &mut remote);
    fs::remove_dir_all(site.root.join("app")).unwrap();
    probe_r6_settle(&mut b, &mut remote);
    assert!(!remote.tree().contains_key(".env"), "{:?}", remote.tree());
}

#[test]
fn probe_r6_move_env_out_then_the_folder_is_deleted_elsewhere() {
    let (mut remote, site_a, mut a, site, mut b) = probe_taken_over_without_rules();
    fs::rename(site.root.join("app/.env"), site.root.join(".env")).unwrap();
    probe_r6_settle(&mut b, &mut remote);
    fs::remove_dir_all(site_a.root.join("app")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    for _ in 0..3 {
        b.pull(&remote);
        b.run(&mut remote);
        b.settle();
    }
    assert!(!remote.tree().contains_key(".env"), "{:?}", remote.tree());
}

#[test]
fn probe_r6_rename_delete_then_recreate() {
    let (mut remote, _, _, site, mut b) = probe_taken_over_without_rules();
    fs::rename(site.root.join("app/.env"), site.root.join("app/settings.txt")).unwrap();
    probe_r6_settle(&mut b, &mut remote);
    fs::remove_file(site.root.join("app/settings.txt")).unwrap();
    probe_r6_settle(&mut b, &mut remote);
    fs::write(site.root.join("app/.env"), b"SECRET=new").unwrap();
    probe_r6_settle(&mut b, &mut remote);
    assert!(!remote.tree().contains_key("app/.env"), "{:?}", remote.tree());
}

#[test]
fn probe_r6_restart_with_a_bound_node() {
    let (mut remote, _, _, site, mut b) = probe_taken_over_without_rules();
    fs::rename(site.root.join("app/.env"), site.root.join("app/settings.txt")).unwrap();
    probe_r6_settle(&mut b, &mut remote);
    drop(b);
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    fs::write(site.root.join("app/settings.txt"), b"SECRET=after restart").unwrap();
    probe_r6_settle(&mut b, &mut remote);
    fs::rename(site.root.join("app/settings.txt"), site.root.join("app/conf.txt")).unwrap();
    probe_r6_settle(&mut b, &mut remote);
    assert!(!remote.tree().contains_key("app/settings.txt"), "{:?}", remote.tree());
    assert!(!remote.tree().contains_key("app/conf.txt"), "{:?}", remote.tree());
}

#[test]
fn probe_r6_a_writebackup_save_seen_in_one_scan() {
    let (mut remote, _, _, site, mut b) = probe_taken_over_without_rules();
    fs::rename(site.root.join("app/.env"), site.root.join("app/.env~")).unwrap();
    fs::write(site.root.join("app/.env"), b"SECRET=saved").unwrap();
    fs::remove_file(site.root.join("app/.env~")).unwrap();
    probe_r6_settle(&mut b, &mut remote);
    assert!(!remote.tree().contains_key("app/.env"), "{:?}", remote.tree());
}

#[test]
fn probe_r6_move_env_into_a_new_synced_folder() {
    let (mut remote, _, _, site, mut b) = probe_taken_over_without_rules();
    fs::create_dir(site.root.join("app/config")).unwrap();
    fs::rename(site.root.join("app/.env"), site.root.join("app/config/.env")).unwrap();
    fs::write(site.root.join("app/config/readme.txt"), b"synced").unwrap();
    probe_r6_settle(&mut b, &mut remote);
    fs::write(site.root.join("app/config/.env"), b"SECRET=edited after the create").unwrap();
    probe_r6_settle(&mut b, &mut remote);
    assert!(remote.tree().contains_key("app/config/readme.txt"), "{:?}", remote.tree());
    assert!(!remote.tree().contains_key("app/config/.env"), "{:?}", remote.tree());
}

fn probe_r6b_pull(b: &mut Device, remote: &mut Remote) {
    for _ in 0..3 {
        b.pull(remote);
        b.run(remote);
        b.settle();
    }
}

#[test]
fn probe_r6b_env_moved_into_a_synced_folder_that_is_deleted_and_taken_over_elsewhere() {
    let (mut remote, site_a, mut a, site, mut b) = probe_taken_over_without_rules();
    fs::create_dir(site_a.root.join("docs")).unwrap();
    a.create(&mut remote, &site_a.root.join("docs/README.md"), b"docs");
    probe_r6b_pull(&mut b, &mut remote);
    fs::rename(site.root.join("app/.env"), site.root.join("docs/.env")).unwrap();
    probe_r6_settle(&mut b, &mut remote);
    assert!(!remote.tree().contains_key("docs/.env"), "{:?}", remote.tree());
    fs::remove_dir_all(site_a.root.join("docs")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    probe_r6b_pull(&mut b, &mut remote);
    assert!(!remote.tree().contains_key("docs/.env"), "{:?}", remote.tree());
    fs::create_dir(site_a.root.join("docs")).unwrap();
    a.create(&mut remote, &site_a.root.join("docs/README.md"), b"docs again");
    probe_r6b_pull(&mut b, &mut remote);
    fs::write(site.root.join("docs/.env"), b"SECRET=after the second takeover").unwrap();
    probe_r6_settle(&mut b, &mut remote);
    assert!(!remote.tree().contains_key("docs/.env"), "{:?}", remote.tree());
    assert_eq!(fs::read(site.root.join("docs/.env")).unwrap(), b"SECRET=after the second takeover");
    assert!(nothing_parked(&site), "{:?}", files(&site.root));
}

#[test]
fn probe_renaming_a_kept_rules_file() {
    let (mut remote, _, _, site, mut b, _) =
        deleted_with_rules_files(&[(".koiignore", ".env\n")], &["README.md"], &[".env"]);
    fs::rename(site.root.join("app/.koiignore"), site.root.join("app/notes.txt")).unwrap();
    for _ in 0..3 {
        b.settle();
        b.run(&mut remote);
    }
    let tree = remote.tree();
    assert!(["app", "app/.env", "app/notes.txt"].iter().all(|path| tree.contains_key(*path)), "{tree:?}");
    assert!(!tree.contains_key("app/.koiignore"), "{tree:?}");
    assert_eq!(b.folder.view().problem, None);
    assert!(b.folder.view().kept_entries.is_empty());
}

#[test]
fn probe_r6b_moved_out_and_folder_deleted_then_restart() {
    let (mut remote, _, _, site, mut b) = probe_taken_over_without_rules();
    fs::rename(site.root.join("app/.env"), site.root.join(".env")).unwrap();
    probe_r6_settle(&mut b, &mut remote);
    fs::remove_dir_all(site.root.join("app")).unwrap();
    probe_r6_settle(&mut b, &mut remote);
    drop(b);
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    fs::write(site.root.join(".env"), b"SECRET=after restart").unwrap();
    probe_r6_settle(&mut b, &mut remote);
    assert!(!remote.tree().contains_key(".env"), "{:?}", remote.tree());
    assert_eq!(b.folder.view().kept_entries, [koidrive_fs::ItemPath::from(".env")]);
}

#[test]
fn probe_r6b_two_hops_with_both_folders_deleted() {
    let (mut remote, _, _, site, mut b) = probe_taken_over_without_rules();
    fs::create_dir(site.root.join("docs")).unwrap();
    fs::write(site.root.join("docs/README.md"), b"docs").unwrap();
    probe_r6_settle(&mut b, &mut remote);
    fs::rename(site.root.join("app/.env"), site.root.join("docs/.env")).unwrap();
    probe_r6_settle(&mut b, &mut remote);
    fs::remove_dir_all(site.root.join("app")).unwrap();
    probe_r6_settle(&mut b, &mut remote);
    fs::rename(site.root.join("docs/.env"), site.root.join(".env")).unwrap();
    probe_r6_settle(&mut b, &mut remote);
    fs::remove_dir_all(site.root.join("docs")).unwrap();
    probe_r6_settle(&mut b, &mut remote);
    fs::write(site.root.join(".env"), b"SECRET=two hops").unwrap();
    probe_r6_settle(&mut b, &mut remote);
    assert!(!remote.tree().contains_key(".env"), "{:?}", remote.tree());
    assert!(!remote.tree().contains_key("docs/.env"), "{:?}", remote.tree());
}

#[test]
fn probe_r6c_takeover_alone() {
    let (mut remote, site_a, mut a, site, mut b) = probe_taken_over_without_rules();
    fs::create_dir(site_a.root.join("docs")).unwrap();
    a.create(&mut remote, &site_a.root.join("docs/README.md"), b"docs");
    probe_r6b_pull(&mut b, &mut remote);
    fs::rename(site.root.join("app/.env"), site.root.join("docs/.env")).unwrap();
    probe_r6_settle(&mut b, &mut remote);
    fs::remove_dir_all(site_a.root.join("docs")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    probe_r6b_pull(&mut b, &mut remote);
    fs::create_dir(site_a.root.join("docs")).unwrap();
    a.create(&mut remote, &site_a.root.join("docs/README.md"), b"docs again");
    probe_r6b_pull(&mut b, &mut remote);
    assert!(!remote.tree().contains_key("docs/.env"), "{:?}", remote.tree());
}
