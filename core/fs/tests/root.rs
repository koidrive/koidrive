// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::os::fd::AsFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use koidrive_fs::{
    Marker, PRIVATE_DIR, RootAt, RootProblem, RootTokens, Space, Unresolved, VolumeId, check_root,
    check_root_at, new_token, open_root_folder, probe_no_replace, read_marker, rename_no_replace,
    root_at, write_marker,
};

fn marker(space_id: &str, effects: u64) -> Marker {
    Marker {
        space_id: space_id.into(),
        effects,
        token: None,
    }
}

fn private_names(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(root.join(PRIVATE_DIR))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[test]
fn marker_round_trips_and_replaces_a_stale_temporary_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    assert_eq!(read_marker(root).unwrap(), None);

    write_marker(root, &marker("space-1", 0)).unwrap();
    assert_eq!(read_marker(root).unwrap(), Some(marker("space-1", 0)));

    fs::write(
        root.join(PRIVATE_DIR).join("space.json.tmp"),
        "left by a crash",
    )
    .unwrap();
    write_marker(root, &marker("space-1", 7)).unwrap();
    assert_eq!(read_marker(root).unwrap(), Some(marker("space-1", 7)));
    assert_eq!(private_names(root), ["space.json"]);
}

#[test]
fn a_marker_without_a_token_reads_and_writes_back_as_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let file = root.join(PRIVATE_DIR).join("space.json");
    let unversioned = serde_json::json!({ "space_id": "space-1", "effects": 4 });
    fs::create_dir(root.join(PRIVATE_DIR)).unwrap();
    fs::write(&file, unversioned.to_string()).unwrap();
    let read = read_marker(root).unwrap().unwrap();
    assert_eq!(read, marker("space-1", 4));
    write_marker(root, &read).unwrap();
    let written: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    assert_eq!(written, unversioned);

    let token = new_token();
    assert!(token.len() == 32 && token.chars().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(new_token(), token);
    let tokened = Marker {
        token: Some(token),
        ..read
    };
    write_marker(root, &tokened).unwrap();
    assert_eq!(read_marker(root).unwrap(), Some(tokened));
}

#[test]
fn a_link_admits_the_token_it_wrote_or_is_writing_and_any_before_its_first() {
    let carrying = |token: Option<&str>| Marker {
        token: token.map(str::to_owned),
        ..marker("space-1", 0)
    };
    let tokens = |current: Option<&str>, pending: Option<&str>| RootTokens {
        current: current.map(str::to_owned),
        pending: pending.map(str::to_owned),
    };
    let cases = [
        (
            tokens(None, None),
            [None, Some("a")].as_slice(),
            [].as_slice(),
        ),
        (tokens(None, Some("b")), &[None, Some("a"), Some("b")], &[]),
        (tokens(Some("a"), None), &[Some("a")], &[None, Some("b")]),
        (
            tokens(Some("a"), Some("b")),
            &[Some("a"), Some("b")],
            &[None, Some("c")],
        ),
    ];
    for (tokens, admitted, copies) in cases {
        for token in admitted {
            assert!(
                tokens.admits(&carrying(*token)).is_ok(),
                "{tokens:?} {token:?}"
            );
        }
        for token in copies {
            assert!(
                matches!(tokens.admits(&carrying(*token)), Err(RootProblem::Copy)),
                "{tokens:?} {token:?}"
            );
        }
    }
}

#[test]
fn check_root_names_each_problem() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    let linked = RootAt {
        dev: fs::metadata(dir.path()).unwrap().dev(),
        volume: None,
    };

    assert!(matches!(
        check_root(&root, "space-1", &linked),
        Err(RootProblem::Missing)
    ));
    fs::write(&root, "a file").unwrap();
    assert!(matches!(
        check_root(&root, "space-1", &linked),
        Err(RootProblem::NotAFolder)
    ));
    fs::remove_file(&root).unwrap();
    fs::create_dir(&root).unwrap();
    assert!(matches!(
        check_root(&root, "space-1", &linked),
        Err(RootProblem::NoMarker)
    ));
    fs::create_dir(root.join(PRIVATE_DIR)).unwrap();
    fs::write(root.join(PRIVATE_DIR).join("space.json"), "{").unwrap();
    assert!(matches!(
        check_root(&root, "space-1", &linked),
        Err(RootProblem::BadMarker(_))
    ));
    write_marker(&root, &marker("space-2", 3)).unwrap();
    assert!(matches!(
        check_root(&root, "space-1", &linked),
        Err(RootProblem::OtherSpace(id)) if id == "space-2"
    ));
    assert!(matches!(
        check_root(
            &root,
            "space-2",
            &RootAt {
                dev: linked.dev.wrapping_add(1),
                volume: None
            }
        ),
        Err(RootProblem::Renumbered)
    ));
    assert_eq!(
        check_root(&root, "space-2", &linked).unwrap(),
        marker("space-2", 3)
    );

    fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).unwrap();
    let readable = fs::read_dir(&root).is_ok();
    let result = check_root(&root, "space-2", &linked);
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    if !readable {
        assert!(
            matches!(result, Err(RootProblem::Unreadable(_))),
            "{result:?}"
        );
    }
}

fn volume(byte: u8) -> Option<VolumeId> {
    Some(VolumeId::Uuid([byte; 16]))
}

#[test]
fn a_root_on_a_volume_with_a_uuid_is_admitted_under_any_device_number() {
    let linked = RootAt {
        dev: 16_777_231,
        volume: volume(1),
    };
    for dev in [16_777_231, 16_777_234] {
        let found = RootAt { dev, ..linked };
        assert!(linked.admits(&found).is_ok(), "dev {dev}");
    }
}

#[test]
fn a_root_on_another_volume_is_refused() {
    let linked = RootAt {
        dev: 16_777_231,
        volume: volume(1),
    };
    let found = |dev, volume| RootAt { dev, volume };
    assert!(matches!(
        linked.admits(&found(16_777_231, volume(2))),
        Err(RootProblem::OtherDisk)
    ));
    assert!(matches!(
        linked.admits(&found(16_777_231, None)),
        Err(RootProblem::OtherDisk)
    ));
}

#[test]
fn a_root_linked_without_a_volume_uuid_is_admitted_only_under_its_device_number() {
    let linked = RootAt {
        dev: 16_777_231,
        volume: None,
    };
    for found in [None, volume(1)] {
        let same = RootAt {
            dev: 16_777_231,
            volume: found,
        };
        assert!(linked.admits(&same).is_ok());
        let renumbered = RootAt {
            dev: 16_777_234,
            volume: found,
        };
        assert!(matches!(
            linked.admits(&renumbered),
            Err(RootProblem::Renumbered)
        ));
    }
}

#[cfg(target_vendor = "apple")]
#[test]
fn a_root_on_apfs_has_the_uuid_of_its_volume() {
    let dir = tempfile::tempdir().unwrap();
    let found = root_at(fs::File::open(dir.path()).unwrap().as_fd()).unwrap();
    assert!(found.volume.is_some(), "APFS reports a volume UUID");
    assert_eq!(found.dev, fs::metadata(dir.path()).unwrap().dev());
    let again = root_at(fs::File::open(dir.path()).unwrap().as_fd()).unwrap();
    assert_eq!(again, found);
    write_marker(dir.path(), &marker("space-1", 0)).unwrap();
    let renumbered = RootAt {
        dev: found.dev.wrapping_add(3),
        ..found
    };
    assert_eq!(
        check_root(dir.path(), "space-1", &renumbered).unwrap(),
        marker("space-1", 0)
    );
}

#[test]
fn a_volume_id_keeps_its_kind_in_its_length() {
    let uuid = VolumeId::Uuid([1; 16]);
    let file_system = VolumeId::FileSystemId([1; 8]);
    assert_eq!(uuid.as_bytes().len(), 16);
    assert_eq!(file_system.as_bytes().len(), 8);
    assert_eq!(VolumeId::from_bytes(uuid.as_bytes()), Some(uuid));
    assert_eq!(
        VolumeId::from_bytes(file_system.as_bytes()),
        Some(file_system)
    );
    for len in [0, 1, 7, 9, 15, 17, 32] {
        assert_eq!(VolumeId::from_bytes(&vec![1; len]), None, "{len} bytes");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn a_root_on_ext4_or_btrfs_has_a_volume_id_and_on_xfs_from_linux_6_9() {
    let uname = rustix::system::uname();
    let release = uname.release().to_string_lossy().into_owned();
    let mut numbers = release.split(|c: char| !c.is_ascii_digit());
    let mut next = || {
        numbers
            .next()
            .and_then(|n| n.parse::<u32>().ok())
            .unwrap_or(0)
    };
    let version = (next(), next());
    let dir = tempfile::tempdir().unwrap();
    let file_system = rustix::fs::statfs(dir.path()).unwrap().f_type as u64;
    let found = root_at(fs::File::open(dir.path()).unwrap().as_fd()).unwrap();
    let context = format!("linux {release} on {file_system:#x}: {:?}", found.volume);
    match file_system {
        0xEF53 => assert!(
            matches!(found.volume, Some(VolumeId::FileSystemId(_))),
            "{context}"
        ),
        0x9123_683E => assert!(matches!(found.volume, Some(VolumeId::Uuid(_))), "{context}"),
        0x5846_5342 if version >= (6, 9) => {
            assert!(matches!(found.volume, Some(VolumeId::Uuid(_))), "{context}")
        }
        _ => {}
    }
}

#[cfg(target_os = "linux")]
const LOOP_MOUNTS: &str = "KOIDRIVE_TEST_LOOP_MOUNTS";

#[cfg(target_os = "linux")]
#[test]
fn an_ext4_disk_attached_again_under_another_device_number_is_the_same_volume() {
    use std::io::Write;
    use std::process::Command;

    if std::env::var_os(LOOP_MOUNTS).is_none() {
        let _ = writeln!(
            std::io::stderr(),
            "SKIPPED an_ext4_disk_attached_again_under_another_device_number_is_the_same_volume: \
             {LOOP_MOUNTS} is not set"
        );
        return;
    }
    let privileged = |program: &str| {
        if rustix::process::geteuid().is_root() {
            Command::new(program)
        } else {
            let mut command = Command::new("sudo");
            command.arg(program);
            command
        }
    };
    let run = |program: &str, args: &[&std::ffi::OsStr]| {
        let output = privileged(program).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{program} {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    struct Attached(std::path::PathBuf, Vec<String>);
    impl Drop for Attached {
        fn drop(&mut self) {
            let quiet = |program: &str, args: &[&std::ffi::OsStr]| {
                let mut command = if rustix::process::geteuid().is_root() {
                    Command::new(program)
                } else {
                    let mut command = Command::new("sudo");
                    command.arg(program);
                    command
                };
                let _ = command.args(args).output();
            };
            quiet("umount", &[self.0.as_ref()]);
            for device in &self.1 {
                quiet("losetup", &["-d".as_ref(), device.as_ref()]);
            }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let image = |name: &str| {
        let path = dir.path().join(name);
        fs::File::create(&path).unwrap().set_len(16 << 20).unwrap();
        let status = Command::new("mkfs.ext4")
            .args(["-q", "-F"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        path
    };
    let a = image("a.img");
    let b = image("b.img");
    let mounted = dir.path().join("mounted");
    fs::create_dir(&mounted).unwrap();
    let mut attached = Attached(mounted.clone(), Vec::new());
    let at = |device: &str| {
        run("mount", &[device.as_ref(), mounted.as_ref()]);
        let found = root_at(fs::File::open(&mounted).unwrap().as_fd()).unwrap();
        run("umount", &[mounted.as_ref()]);
        found
    };

    let first = run(
        "losetup",
        &["--find".as_ref(), "--show".as_ref(), a.as_ref()],
    );
    attached.1.push(first.clone());
    let linked = at(&first);
    let free = run("losetup", &["--find".as_ref()]);
    run("losetup", &["-d".as_ref(), first.as_ref()]);
    run("losetup", &[free.as_ref(), a.as_ref()]);
    attached.1.push(free.clone());
    let again = at(&free);
    let other_device = run(
        "losetup",
        &["--find".as_ref(), "--show".as_ref(), b.as_ref()],
    );
    attached.1.push(other_device.clone());
    let other = at(&other_device);

    assert!(
        matches!(linked.volume, Some(VolumeId::FileSystemId(_))),
        "{linked:?}"
    );
    assert_ne!(linked.dev, again.dev);
    assert!(linked.admits(&again).is_ok(), "{linked:?} {again:?}");
    assert!(
        matches!(linked.admits(&other), Err(RootProblem::OtherDisk)),
        "{linked:?} {other:?}"
    );
}

#[test]
fn a_held_root_is_checked_whatever_its_path_names_now() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    let other = dir.path().join("other");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&other).unwrap();
    write_marker(&root, &marker("space-1", 1)).unwrap();
    write_marker(&other, &marker("space-1", 2)).unwrap();
    let held = open_root_folder(&root).unwrap();
    let linked = root_at(held.as_fd()).unwrap();

    fs::rename(&root, dir.path().join("away")).unwrap();
    fs::rename(&other, &root).unwrap();

    assert_eq!(
        check_root_at(held.as_fd(), "space-1", &linked).unwrap(),
        marker("space-1", 1)
    );
    assert_eq!(
        check_root(&root, "space-1", &linked).unwrap(),
        marker("space-1", 2)
    );
}

#[cfg(target_os = "linux")]
const BTRFS_DIR: &str = "KOIDRIVE_TEST_BTRFS_DIR";

#[cfg(target_os = "linux")]
#[test]
fn a_btrfs_snapshot_rolled_back_into_the_path_is_another_root() {
    use std::io::Write;
    use std::process::Command;

    let Some(dir) = std::env::var_os(BTRFS_DIR) else {
        let _ = writeln!(
            std::io::stderr(),
            "SKIPPED a_btrfs_snapshot_rolled_back_into_the_path_is_another_root: \
             {BTRFS_DIR} names no folder on btrfs"
        );
        return;
    };
    let base = tempfile::tempdir_in(dir).unwrap();
    let subvolume = base.path().join("subvolume");
    let snapshot = base.path().join("snapshot");
    let btrfs = |args: &[&std::ffi::OsStr]| {
        let status = Command::new("btrfs").args(args).status().unwrap();
        assert!(status.success(), "btrfs {args:?}");
    };
    let at = |path: &Path| root_at(fs::File::open(path).unwrap().as_fd()).unwrap();
    btrfs(&["subvolume".as_ref(), "create".as_ref(), subvolume.as_ref()]);
    fs::create_dir(subvolume.join("folder")).unwrap();
    let token = new_token();
    let tokened = Marker {
        token: Some(token.clone()),
        ..marker("space-1", 0)
    };
    write_marker(&subvolume.join("folder"), &tokened).unwrap();
    let tokens = RootTokens {
        current: Some(token),
        pending: None,
    };
    let linked_subvolume = at(&subvolume);
    let linked = at(&subvolume.join("folder"));
    assert!(linked.volume.is_some());
    assert!(linked.admits(&at(&subvolume.join("folder"))).is_ok());

    btrfs(&[
        "subvolume".as_ref(),
        "snapshot".as_ref(),
        subvolume.as_ref(),
        snapshot.as_ref(),
    ]);
    fs::rename(&subvolume, base.path().join("replaced")).unwrap();
    fs::rename(&snapshot, &subvolume).unwrap();

    let rolled_back = read_marker(&subvolume.join("folder")).unwrap().unwrap();
    assert_eq!(rolled_back, tokened);
    assert!(tokens.admits(&rolled_back).is_ok());
    assert!(matches!(
        check_root(&subvolume.join("folder"), "space-1", &linked),
        Err(RootProblem::OtherDisk)
    ));
    assert!(matches!(
        linked.admits(&at(&subvolume.join("folder"))),
        Err(RootProblem::OtherDisk)
    ));
    assert!(matches!(
        linked_subvolume.admits(&at(&subvolume)),
        Err(RootProblem::OtherDisk)
    ));
    assert!(
        linked
            .admits(&at(&base.path().join("replaced").join("folder")))
            .is_ok()
    );
}

#[test]
fn marker_behind_a_symlink_is_not_read() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    let outside = dir.path().join("outside");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&outside).unwrap();
    write_marker(&outside, &marker("space-1", 0)).unwrap();
    std::os::unix::fs::symlink(outside.join(PRIVATE_DIR), root.join(PRIVATE_DIR)).unwrap();
    let linked = RootAt {
        dev: fs::metadata(&root).unwrap().dev(),
        volume: None,
    };

    assert!(read_marker(&root).is_err());
    assert!(matches!(
        check_root(&root, "space-1", &linked),
        Err(RootProblem::BadMarker(_))
    ));
}

#[test]
fn rename_never_replaces_its_target() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    assert!(probe_no_replace(root).unwrap());
    assert!(private_names(root).is_empty());

    fs::write(root.join("a"), "a").unwrap();
    fs::write(root.join("b"), "b").unwrap();
    let here = fs::File::open(root).unwrap();
    let err =
        rename_no_replace(here.as_fd(), "a".as_ref(), here.as_fd(), "b".as_ref()).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read_to_string(root.join("a")).unwrap(), "a");
    assert_eq!(fs::read_to_string(root.join("b")).unwrap(), "b");

    rename_no_replace(here.as_fd(), "a".as_ref(), here.as_fd(), "c".as_ref()).unwrap();
    assert!(!root.join("a").exists());
    assert_eq!(fs::read_to_string(root.join("c")).unwrap(), "a");
}

/// A folder on a file system whose rename refuses the no-replace flag, such
/// as a FUSE file system without `RENAME_NOREPLACE`. Linux checks an existing
/// target before the file system sees the flag, so only a rename to a free
/// name shows the gap.
const NO_REPLACE_FREE_DIR: &str = "KOIDRIVE_TEST_NO_REPLACE_FREE_DIR";

#[test]
fn probe_refuses_a_file_system_whose_rename_lacks_no_replace() {
    use std::io::Write;

    let Some(dir) = std::env::var_os(NO_REPLACE_FREE_DIR) else {
        let _ = writeln!(
            std::io::stderr(),
            "SKIPPED probe_refuses_a_file_system_whose_rename_lacks_no_replace: \
             {NO_REPLACE_FREE_DIR} names no folder on such a file system"
        );
        return;
    };
    let root = Path::new(&dir).join(format!("probe-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    assert!(!probe_no_replace(&root).unwrap());
}
#[test]
fn marker_and_probe_go_through_the_held_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    let moved = dir.path().join("moved");
    fs::create_dir(&root).unwrap();
    let space = Space::bind(fs::File::open(&root).unwrap().into(), &root).unwrap();
    assert_eq!(space.marker().unwrap(), None);
    assert!(!root.join(PRIVATE_DIR).exists());
    assert!(space.probe_no_replace().unwrap());
    assert!(private_names(&root).is_empty());

    write_marker(&root, &marker("space-1", 3)).unwrap();
    assert_eq!(space.marker().unwrap(), Some(marker("space-1", 3)));
    assert_eq!(private_names(&root), ["space.json"]);

    fs::rename(&root, &moved).unwrap();
    fs::create_dir(&root).unwrap();
    write_marker(&root, &marker("space-2", 9)).unwrap();
    assert_eq!(space.marker().unwrap(), Some(marker("space-1", 3)));
    let err = space.probe_no_replace().unwrap_err();
    assert!(
        matches!(
            err.get_ref()
                .and_then(|err| err.downcast_ref::<Unresolved>()),
            Some(Unresolved::RootWithdrawn(_))
        ),
        "{err}"
    );
    assert_eq!(private_names(&root), ["space.json"]);
    assert_eq!(private_names(&moved), ["space.json"]);
}
