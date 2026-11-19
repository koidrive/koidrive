// SPDX-License-Identifier: Apache-2.0

mod common;

use std::fs;
use std::path::Path;
#[cfg(target_vendor = "apple")]
use std::process::Command;

use common::{apply_one, at, bind, dir, entries, prepare, scenario};
use koidrive_fs::{Location, Outcome, Snapshot};

#[cfg(target_vendor = "apple")]
const NAME: &str = "com.koidrive.node";
#[cfg(not(target_vendor = "apple"))]
const NAME: &str = "user.koidrive.node";

fn location(root: &Path, path: &str) -> Location {
    let names: Vec<&str> = path.split('/').collect();
    let (name, folders) = names.split_last().unwrap();
    let dirs = (1..=folders.len())
        .map(|depth| {
            dir(
                folders[depth - 1],
                &entries(root, &[&folders[..depth].join("/")])[0],
            )
        })
        .collect();
    at(dirs, name)
}

fn mark_of(root: &Path, path: &str) -> Option<String> {
    let id = entries(root, &[path])[0].id;
    bind(root).mark_of(&location(root, path), id).unwrap()
}

fn mark(root: &Path, path: &str, node: &str) {
    let id = entries(root, &[path])[0].id;
    bind(root).mark(&location(root, path), id, node).unwrap();
}

fn copy_attribute(from: &Path, to: &Path) {
    let mut value = [0; 256];
    let len = rustix::fs::getxattr(from, NAME, &mut value).unwrap();
    rustix::fs::setxattr(to, NAME, &value[..len], rustix::fs::XattrFlags::empty()).unwrap();
}

#[test]
fn a_placed_file_and_a_created_folder_carry_the_mark_of_their_node() {
    for (name, path) in [
        ("write_new", "docs/new.txt"),
        ("replace", "docs/report.txt"),
        ("create_dir", "docs/sub"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let effect = prepare(scenario(name), root);
        let outcome = apply_one(&bind(root), &effect, &mut |_| {}).unwrap();
        assert!(
            matches!(outcome, Outcome::Done { .. }),
            "{name}: {outcome:?}"
        );
        assert_eq!(mark_of(root, path).as_deref(), Some("node"), "{name}");
    }
}

#[test]
fn a_mark_written_under_another_device_number_still_names_its_node() {
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = prepare(scenario("write_new"), root);
    apply_one(&bind(root), &effect, &mut |_| {}).unwrap();
    let path = root.join("docs/new.txt");
    let meta = fs::symlink_metadata(&path).unwrap();
    let birth = entries(root, &["docs/new.txt"])[0]
        .id
        .birth_ns
        .map_or_else(|| "-".to_owned(), |birth| birth.to_string());
    let renumbered = format!("1 node {} {} {birth}", meta.dev() + 3, meta.ino());
    rustix::fs::setxattr(
        &path,
        NAME,
        renumbered.as_bytes(),
        rustix::fs::XattrFlags::empty(),
    )
    .unwrap();
    assert_eq!(mark_of(root, "docs/new.txt").as_deref(), Some("node"));
    let other_file = format!("1 node {} {} {birth}", meta.dev(), meta.ino() + 1);
    rustix::fs::setxattr(
        &path,
        NAME,
        other_file.as_bytes(),
        rustix::fs::XattrFlags::empty(),
    )
    .unwrap();
    assert_eq!(mark_of(root, "docs/new.txt"), None);
}

#[test]
fn a_moved_entry_keeps_its_mark_and_a_copy_of_it_has_none() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = prepare(scenario("write_new"), root);
    apply_one(&bind(root), &effect, &mut |_| {}).unwrap();
    fs::rename(root.join("docs/new.txt"), root.join("moved.txt")).unwrap();
    fs::copy(root.join("moved.txt"), root.join("copy.txt")).unwrap();
    copy_attribute(&root.join("moved.txt"), &root.join("copy.txt"));
    assert_eq!(mark_of(root, "moved.txt").as_deref(), Some("node"));
    assert_eq!(mark_of(root, "copy.txt"), None);
}

#[test]
fn a_mark_changes_nothing_a_scan_compares() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("docs")).unwrap();
    fs::write(root.join("docs/a.txt"), b"bytes").unwrap();
    let before = Snapshot::scan(root).unwrap().entries;
    let modified = fs::metadata(root.join("docs/a.txt"))
        .unwrap()
        .modified()
        .unwrap();
    mark(root, "docs/a.txt", "file");
    mark(root, "docs", "folder");
    assert_eq!(Snapshot::scan(root).unwrap().entries, before);
    let after = fs::metadata(root.join("docs/a.txt"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(after, modified);
    assert_eq!(fs::read(root.join("docs/a.txt")).unwrap(), b"bytes");
    assert_eq!(mark_of(root, "docs/a.txt").as_deref(), Some("file"));
    assert_eq!(mark_of(root, "docs").as_deref(), Some("folder"));
}

#[test]
fn an_entry_that_took_the_path_since_gets_no_mark() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("a.txt"), b"first").unwrap();
    let first = entries(root, &["a.txt"])[0].id;
    fs::write(root.join("b.txt"), b"second").unwrap();
    fs::rename(root.join("b.txt"), root.join("a.txt")).unwrap();
    let space = bind(root);
    space
        .mark(&location(root, "a.txt"), first, "first")
        .unwrap();
    assert_eq!(
        space.mark_of(&location(root, "a.txt"), first).unwrap(),
        None
    );
    assert_eq!(mark_of(root, "a.txt"), None);
}

#[cfg(target_vendor = "apple")]
#[test]
fn a_volume_that_keeps_attributes_in_companion_files_gets_no_mark() {
    let mut disks = koidrive_test_support::Disks::new().unwrap();
    let image = disks.fat("KOIMARK");
    let mounted = disks.base().join("fat");
    fs::create_dir(&mounted).unwrap();
    disks.attach(&image, &mounted);
    let root = mounted.as_path();
    fs::create_dir(root.join("docs")).unwrap();
    fs::write(root.join("docs/a.txt"), b"bytes").unwrap();
    fs::write(root.join("docs/control.txt"), b"bytes").unwrap();
    let companions = || -> Vec<(std::path::PathBuf, Vec<u8>)> {
        let mut found = Vec::new();
        for folder in [root.to_path_buf(), root.join("docs")] {
            for entry in fs::read_dir(&folder).unwrap() {
                let path = entry.unwrap().path();
                if path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("._")
                {
                    found.push((path.clone(), fs::read(&path).unwrap()));
                }
            }
        }
        found.sort();
        found
    };
    let before = companions();
    mark(root, "docs/a.txt", "file");
    mark(root, "docs", "folder");
    assert_eq!(mark_of(root, "docs/a.txt"), None);
    assert_eq!(mark_of(root, "docs"), None);
    assert_eq!(companions(), before);
    let control = root.join("docs/control.txt");
    rustix::fs::setxattr(&control, NAME, b"1 node", rustix::fs::XattrFlags::empty()).unwrap();
    let changed: Vec<_> = (companions().into_iter())
        .filter(|(path, bytes)| !before.iter().any(|(was, old)| was == path && old == bytes))
        .map(|(path, _)| path)
        .collect();
    assert_eq!(changed, [root.join("docs/._control.txt")]);
}

#[cfg(target_vendor = "apple")]
#[test]
fn a_replacement_whose_mark_cannot_be_written_is_placed_unmarked() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let scenario = scenario("replace");
    let effect = prepare(scenario, root);
    let staged = root.join(".koidrive/staging").join(&effect.id);
    let denied = Command::new("chmod")
        .args(["+a", "everyone deny writeextattr"])
        .arg(&staged)
        .status()
        .unwrap();
    assert!(denied.success());
    let outcome = apply_one(&bind(root), &effect, &mut |_| {}).unwrap();
    assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
    (scenario.check)(root, &outcome);
    assert_eq!(mark_of(root, "docs/report.txt"), None);
}

#[cfg(target_os = "linux")]
#[test]
fn a_refused_mark_write_removes_the_previous_mark() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("a.txt"), b"bytes").unwrap();
    mark(root, "a.txt", "old-node");
    assert_eq!(mark_of(root, "a.txt").as_deref(), Some("old-node"));
    mark(root, "a.txt", &"x".repeat(131072));
    assert_eq!(mark_of(root, "a.txt"), None);
    assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"bytes");
}

#[test]
fn a_batch_of_marks_marks_each_entry_a_single_mark_would() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for folder in ["a/b", "a/c", "d", "gone", "swapped"] {
        fs::create_dir_all(root.join(folder)).unwrap();
    }
    for file in [
        "a/b/1.txt",
        "a/b/2.txt",
        "a/c/3.txt",
        "d/4.txt",
        "5.txt",
        "gone/6.txt",
        "swapped/7.txt",
    ] {
        fs::write(root.join(file), file).unwrap();
    }
    let paths = [
        "d/4.txt",
        "a/b/2.txt",
        "a",
        "a/c/3.txt",
        "5.txt",
        "a/b/1.txt",
        "gone/6.txt",
        "swapped/7.txt",
    ];
    let marks = paths
        .map(|path| {
            (
                location(root, path),
                entries(root, &[path])[0].id,
                path.to_owned(),
            )
        })
        .to_vec();
    fs::remove_dir_all(root.join("gone")).unwrap();
    fs::rename(root.join("swapped"), root.join("old")).unwrap();
    fs::create_dir(root.join("swapped")).unwrap();
    fs::rename(root.join("old/7.txt"), root.join("swapped/7.txt")).unwrap();
    assert!(bind(root).mark_all(marks).is_empty());
    for path in &paths[..6] {
        assert_eq!(mark_of(root, path).as_deref(), Some(*path), "{path}");
    }
    assert_eq!(mark_of(root, "swapped/7.txt"), None);
}
