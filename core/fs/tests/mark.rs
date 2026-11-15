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
struct Volume(std::path::PathBuf);

#[cfg(target_vendor = "apple")]
impl Drop for Volume {
    fn drop(&mut self) {
        let _ = Command::new("hdiutil")
            .arg("detach")
            .arg(&self.0)
            .arg("-force")
            .status();
    }
}

#[cfg(target_vendor = "apple")]
#[test]
fn a_volume_that_keeps_attributes_in_companion_files_gets_no_mark() {
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("fat.dmg");
    let mounted = dir.path().join("fat");
    fs::create_dir(&mounted).unwrap();
    let created = Command::new("hdiutil")
        .args([
            "create", "-quiet", "-size", "8m", "-fs", "MS-DOS", "-volname", "KOIMARK",
        ])
        .arg(&image)
        .status()
        .unwrap();
    assert!(created.success());
    let attached = Command::new("hdiutil")
        .args(["attach", "-quiet", "-nobrowse", "-noverify", "-mountpoint"])
        .arg(&mounted)
        .arg(&image)
        .status()
        .unwrap();
    assert!(attached.success());
    let volume = Volume(mounted.clone());
    let root = volume.0.as_path();
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
