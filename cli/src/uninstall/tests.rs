// SPDX-License-Identifier: Apache-2.0

use std::os::unix::fs::PermissionsExt;

use super::remove::{Cleaned, folder_data, strip_path_line};
use super::text::text;
use super::*;

const LINE: &str = "case \":$PATH:\" in *:'/home/ada/.local/bin':*) ;; *) export PATH='/home/ada/.local/bin':\"$PATH\" ;; esac; : 'koidrive PATH'";

fn owner(device_id: &str) -> Owner {
    Owner {
        server: "https://koidrive.ada.workers.dev".into(),
        device_id: device_id.into(),
    }
}

fn linked(root: &Path, owner: &Owner) -> PathBuf {
    let private = root.join(".koidrive");
    fs::create_dir_all(private.join("staging")).unwrap();
    fs::write(private.join("space.json"), b"{}").unwrap();
    fs::write(
        private.join("device.json"),
        serde_json::to_vec(owner).unwrap(),
    )
    .unwrap();
    fs::write(private.join("staging/e1"), b"half a download").unwrap();
    fs::write(root.join("Budget.xlsx"), b"numbers").unwrap();
    private
}

#[test]
fn the_path_line_goes_and_the_rest_of_the_profile_stays_with_its_mode() {
    let dir = tempfile::tempdir().unwrap();
    let profile = dir.path().join(".zprofile");
    fs::write(
        &profile,
        format!("export EDITOR=vim\n{LINE}\nalias ll='ls -l'\n"),
    )
    .unwrap();
    fs::set_permissions(&profile, fs::Permissions::from_mode(0o640)).unwrap();

    assert!(strip_path_line(&profile).unwrap());

    assert_eq!(
        fs::read_to_string(&profile).unwrap(),
        "export EDITOR=vim\nalias ll='ls -l'\n"
    );
    assert_eq!(
        fs::metadata(&profile).unwrap().permissions().mode() & 0o777,
        0o640
    );
    assert!(!strip_path_line(&profile).unwrap());
}

#[test]
fn a_profile_that_held_only_what_the_installer_wrote_is_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let zsh = dir.path().join(".zprofile");
    fs::write(&zsh, format!("{LINE}\n")).unwrap();
    let bash = dir.path().join(".bash_profile");
    fs::write(&bash, format!("{BASH_PRELUDE}\n{LINE}\n")).unwrap();
    let fish = dir.path().join("koidrive.fish");
    fs::write(
        &fish,
        "contains -- '/home/ada/.local/bin' $PATH; or set -gx PATH '/home/ada/.local/bin' $PATH; true 'koidrive PATH'\n",
    )
    .unwrap();

    for profile in [&zsh, &bash, &fish] {
        assert!(strip_path_line(profile).unwrap(), "{}", profile.display());
        assert!(!profile.exists(), "{}", profile.display());
    }
}

#[test]
fn a_symlinked_profile_is_edited_through_the_link() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("dotfiles/zprofile");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, format!("{LINE}\n")).unwrap();
    let profile = dir.path().join(".zprofile");
    std::os::unix::fs::symlink(&target, &profile).unwrap();

    assert!(strip_path_line(&profile).unwrap());

    assert!(fs::symlink_metadata(&profile).unwrap().is_symlink());
    assert_eq!(fs::read_to_string(&target).unwrap(), "");
}

#[test]
fn a_folder_loses_its_koidrive_records_and_keeps_every_file() {
    let dir = tempfile::tempdir().unwrap();
    let ours = owner("d1");
    let private = linked(dir.path(), &ours);
    fs::create_dir_all(private.join("trash/2026-11-13")).unwrap();
    fs::create_dir_all(private.join("parked")).unwrap();
    fs::write(private.join("trash.json"), b"{}").unwrap();

    assert_eq!(
        folder_data(&private, Some(&ours)).unwrap(),
        Cleaned::Removed
    );

    assert!(!private.exists());
    assert_eq!(
        fs::read(dir.path().join("Budget.xlsx")).unwrap(),
        b"numbers"
    );
    assert_eq!(
        folder_data(&private, Some(&ours)).unwrap(),
        Cleaned::Nothing
    );
}

#[test]
fn files_in_the_local_trash_and_other_holding_folders_stay() {
    let dir = tempfile::tempdir().unwrap();
    let ours = owner("d1");
    let private = linked(dir.path(), &ours);
    fs::create_dir_all(private.join("trash/2026-11-13")).unwrap();
    fs::write(private.join("trash/2026-11-13/Slides.key"), b"old slides").unwrap();
    fs::create_dir_all(private.join("quarantine")).unwrap();
    fs::write(private.join("quarantine/q1"), b"held aside").unwrap();
    fs::create_dir_all(private.join("parked/empty")).unwrap();
    fs::write(private.join("trash.json"), b"{}").unwrap();

    let cleaned = folder_data(&private, Some(&ours)).unwrap();

    assert_eq!(
        cleaned,
        Cleaned::Kept(vec![
            private.join("quarantine"),
            private.join("trash"),
            private.join("trash.json")
        ])
    );
    assert_eq!(
        fs::read(private.join("trash/2026-11-13/Slides.key")).unwrap(),
        b"old slides"
    );
    assert_eq!(
        fs::read(private.join("quarantine/q1")).unwrap(),
        b"held aside"
    );
    for gone in ["space.json", "device.json", "staging", "parked"] {
        assert!(!private.join(gone).exists(), "{gone}");
    }
}

#[test]
fn a_directory_in_place_of_a_recovery_record_stays() {
    let dir = tempfile::tempdir().unwrap();
    let ours = owner("d1");
    let private = linked(dir.path(), &ours);
    fs::create_dir_all(private.join("brought-back.json")).unwrap();
    fs::write(private.join("brought-back.json/notes"), b"mine").unwrap();

    let cleaned = folder_data(&private, Some(&ours)).unwrap();

    assert_eq!(
        cleaned,
        Cleaned::Kept(vec![private.join("brought-back.json")])
    );
    assert_eq!(
        fs::read(private.join("brought-back.json/notes")).unwrap(),
        b"mine"
    );
}

#[test]
fn a_folder_another_device_links_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let private = linked(dir.path(), &owner("d2"));

    for owner in [Some(owner("d1")), None] {
        assert_eq!(
            folder_data(&private, owner.as_ref()).unwrap(),
            Cleaned::LinkedElsewhere
        );
    }
    assert!(private.join("device.json").exists());
    assert!(private.join("staging/e1").exists());
}

#[test]
fn removing_a_link_never_follows_it() {
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = dir.path().join("Documents");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::write(elsewhere.join("Thesis.docx"), b"chapter 1").unwrap();
    let link = dir.path().join("state");
    std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
    let private = dir.path().join("root/.koidrive");
    fs::create_dir_all(&private).unwrap();
    std::os::unix::fs::symlink(&elsewhere, private.join("trash")).unwrap();

    assert!(remove_path(&link).unwrap());
    assert_eq!(
        folder_data(&private, None).unwrap(),
        Cleaned::Kept(vec![private.join("trash")])
    );

    assert_eq!(
        fs::read(elsewhere.join("Thesis.docx")).unwrap(),
        b"chapter 1"
    );
    assert!(!remove_path(&link).unwrap());
}

#[test]
fn the_summary_names_what_stays_and_how_to_revoke_a_device_still_listed() {
    let data = UninstalledData {
        device: DeviceOutcome::StillListed {
            server: Some("https://koidrive.ada.workers.dev".into()),
            device_id: Some("d1".into()),
            problem: Some("the server did not answer".into()),
        },
        removed: vec![Removed::Service {
            name: "dev.koidrive.koi".into(),
        }],
        brought_back: Vec::new(),
        kept: vec![
            Kept::Folder {
                path: "/srv/Koidrive".into(),
            },
            Kept::FolderData {
                path: "/srv/Koidrive/.koidrive/trash".into(),
            },
        ],
    };

    assert_eq!(
        text(&data),
        "Could not sign this device out: the server did not answer.\n\
         https://koidrive.ada.workers.dev may still list this device. To remove it, run this on another device signed in to https://koidrive.ada.workers.dev: koi device revoke d1\n\
         If you have no other device, add one with koi server add-device --interactive and run the command there.\n\
         Removed the background service dev.koidrive.koi.\n\
         Kept /srv/Koidrive and every file in it.\n\
         Kept /srv/Koidrive/.koidrive/trash, which still holds files. Delete it when you no longer need them.\n"
    );
}

#[test]
fn a_device_id_with_control_characters_reaches_the_terminal_escaped() {
    let id = "d9\u{1b}[31mRED\u{7} x";
    let unconfirmed = |server: &str| UnconfirmedData {
        server: server.into(),
        device_id: id.into(),
        reason: "no answer within 10 seconds".into(),
    };
    let escaped = r"koi device revoke 'd9\u{1b}[31mRED\u{7} x'";
    for device in [
        DeviceOutcome::SignedOut {
            server: "https://files.example.com".into(),
            unconfirmed: Some(unconfirmed("https://files.example.com")),
        },
        DeviceOutcome::StillListed {
            server: Some("https://files.example.com".into()),
            device_id: Some(id.into()),
            problem: None,
        },
    ] {
        let shown = text(&UninstalledData {
            device,
            removed: Vec::new(),
            brought_back: Vec::new(),
            kept: Vec::new(),
        });
        assert!(shown.contains(escaped), "{shown}");
        assert!(!shown.contains(['\u{1b}', '\u{7}']), "{shown}");
    }
    let left = crate::device::still_listed(&koidrive_driver::Unconfirmed {
        server: "https://files.example.com".into(),
        device_id: id.into(),
        reason: "no answer within 10 seconds".into(),
    });
    assert!(left.contains(escaped), "{left}");
    assert!(!left.contains(['\u{1b}', '\u{7}']), "{left}");
}

#[test]
fn set_aside_paths_reach_the_terminal_escaped() {
    let shown = text(&UninstalledData {
        device: DeviceOutcome::NotPaired,
        removed: Vec::new(),
        brought_back: vec!["/Docs/a\u{1b}[31m (recovered 2026-11-18)".into()],
        kept: vec![Kept::SetAside {
            path: "/Docs/.koidrive/parked/n\u{7}1".into(),
        }],
    });
    assert!(!shown.contains(['\u{1b}', '\u{7}']), "{shown}");
}

#[test]
fn only_what_koidrive_writes_leaves_a_state_folder() {
    let entry = |name: &str| Paths::state_entry(Path::new(name));
    assert_eq!(entry(RECORD), Some(StateEntry::File));
    for name in [
        "credential.json",
        "credential.next",
        "pending-claim.json",
        ".pending-claim.json.42.tmp",
        koidrive_state_file::MADE,
        ".credential.next.123.tmp",
        ".credential.json.9.tmp",
        "leaving.next",
        ".tmpA1b2C3",
    ] {
        assert_eq!(entry(name), Some(StateEntry::File), "{name}");
    }
    for name in [
        "wedding.jpg",
        ".credential.next.tmp",
        ".notes.1.tmp",
        "credential.json.bak",
        ".tmp-notes",
        ".tmp-backup",
        ".tmpA1b2C3d",
    ] {
        assert_eq!(entry(name), None, "{name}");
    }
}

#[test]
fn a_state_folder_the_service_unit_creates_goes_whole() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let service =
        crate::service::Service::new(&paths, dir.path(), Path::new("/usr/local/bin/koi")).unwrap();

    service.write_unit().unwrap();

    assert!(is_state(&paths.state_dir));
    let mut run = Run::default();
    run.remove_dir(&paths.state_dir, Paths::state_entry);
    assert!(!paths.state_dir.exists(), "{:?}", run.kept);
}

#[test]
fn only_a_state_folder_koidrive_made_is_state_and_has_its_stages_recovered() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("koidrive");
    koidrive_state_file::create_dir(&state, &state).unwrap();
    let theirs = dir.path().join("Backup");
    fs::create_dir(&theirs).unwrap();
    let staged = [&state, &theirs].map(|folder| {
        let staged = koidrive_state_file::AtomicFile::prepare(&folder.join(RECORD)).unwrap();
        staged.as_file().write_all(b"unpublished state").unwrap();
        rustix::fs::flock(staged.as_file(), rustix::fs::FlockOperation::Unlock).unwrap();
        staged
    });

    assert!(is_state(&state));
    assert!(!is_state(&theirs));
    let mut run = Run::default();
    run.remove_dir(&state, Paths::state_entry);

    assert!(!state.exists(), "{:?}", run.kept);
    assert_eq!(fs::read_dir(&theirs).unwrap().count(), 1);
    drop(staged);
}

#[test]
fn a_state_folder_koidrive_did_not_make_is_named_with_the_command_that_removes_it() {
    assert_eq!(
        kept_text(&[Kept::UnrecordedState {
            path: "/srv/state/koidrive".into(),
        }]),
        "Kept /srv/state/koidrive, because Koidrive has no record of creating it. If it holds only Koidrive's files, delete it with: rm -rf /srv/state/koidrive\n"
    );
}

#[test]
fn a_state_folder_loses_only_entries_of_the_shape_koidrive_writes() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("koidrive");
    let space = "0190f0a0-0000-7000-8000-00000000000a";
    let request = format!("requests/{}.json", "ab".repeat(32));
    let write = |rel: &str| {
        let path = state.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, rel).unwrap();
    };
    let ours = [
        "credential.json".to_owned(),
        ".credential.json.7.tmp".to_owned(),
        format!("spaces/{space}/store.sqlite"),
        format!("spaces/{space}/.tmpA1b2C3"),
        "telemetry/send.lock".to_owned(),
    ];
    let theirs = [
        "lock/keys.txt".to_owned(),
        "systemd".to_owned(),
        "spaces/Photos/.credential.json.7.tmp".to_owned(),
        format!("{request}/draft.txt"),
    ];
    for rel in ours.iter().chain(&theirs) {
        write(rel);
    }

    let mut run = Run::default();
    run.remove_dir(&state, Paths::state_entry);

    assert_eq!(run.problems, Vec::<String>::new());
    for rel in &theirs {
        assert_eq!(fs::read_to_string(state.join(rel)).unwrap(), *rel);
    }
    for rel in &ours {
        assert!(!state.join(rel).exists(), "{rel}");
    }
    let shown = |rel: &str| match rel {
        "" => state.display().to_string(),
        rel => state.join(rel).display().to_string(),
    };
    let mut removed = run.removed.clone();
    removed.sort_by_key(|removed| format!("{removed:?}"));
    let mut expected: Vec<_> = [
        "credential.json",
        ".credential.json.7.tmp",
        &format!("spaces/{space}"),
        "telemetry",
    ]
    .into_iter()
    .map(|rel| Removed::Files { path: shown(rel) })
    .collect();
    expected.sort_by_key(|removed| format!("{removed:?}"));
    assert_eq!(removed, expected);
    let mut kept = run.kept.clone();
    kept.sort_by_key(|kept| format!("{kept:?}"));
    let mut expected: Vec<_> = ["", "requests", "spaces"]
        .into_iter()
        .map(|rel| Kept::Unrecognized { path: shown(rel) })
        .collect();
    expected.sort_by_key(|kept| format!("{kept:?}"));
    assert_eq!(kept, expected);
}

#[test]
fn everything_koidrive_writes_in_a_state_folder_goes_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let space = "0190f0a0-0000-7000-8000-00000000000a";
    let store = paths.store(space).unwrap();
    fs::create_dir_all(store.parent().unwrap()).unwrap();
    let store = koidrive_store::Store::open(store).unwrap();
    let telemetry = koidrive_telemetry::Store::new(&paths.state_dir);
    telemetry.begin_prompt().unwrap();
    let hold = telemetry.hold().unwrap();
    let id = koidrive_ipc::RequestId::parse("r1").unwrap();
    let key = koidrive_ipc::Key::new("folder link", serde_json::json!({}));
    assert!(matches!(
        crate::request::begin(&paths, &id, &key).unwrap(),
        crate::request::Begun::Claimed(_)
    ));
    let service =
        service::Service::new(&paths, dir.path(), Path::new("/usr/local/bin/koi")).unwrap();
    let unit = service.file(service::ServiceManager::Systemd);
    fs::create_dir_all(unit.parent().unwrap()).unwrap();
    fs::write(&unit, service.unit()).unwrap();
    let written = tree(&paths.state_dir);
    drop((store, hold));

    let mut run = Run::default();
    run.remove_dir(&paths.state_dir, Paths::state_entry);

    assert!(!paths.state_dir.exists(), "{written:#?}\n{:#?}", run.kept);
    assert_eq!(run.kept, Vec::new());
    assert_eq!(run.problems, Vec::<String>::new());
}

fn tree(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(tree(&path));
        }
        found.push(path);
    }
    found
}

#[tokio::test]
async fn a_folder_whose_place_is_unknown_gets_no_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    koidrive_state_file::create_dir(&paths.state_dir, &paths.state_dir).unwrap();
    let folder = dir.path().join("Koidrive");
    fs::create_dir(&folder).unwrap();
    let status: koidrive_driver::Status = serde_json::from_value(serde_json::json!({
        "account": null, "running": true,
        "folders": ([folder.to_str().unwrap(), ""].map(|path| serde_json::json!({
            "space_id": "s1", "space_name": "Home", "path": path, "state": "halted",
            "history_id": null, "last_seq": null, "head_seq": null, "remote_nodes": 0,
            "local_entries": null, "questions": 0
        })))
    }))
    .unwrap();
    let listener = koidrive_ipc::bind(&paths.socket()).unwrap();
    let served = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        koidrive_ipc::read_line(&mut stream).await.unwrap();
        koidrive_ipc::write_line(&mut stream, &Response::Status { status })
            .await
            .unwrap();
    });

    let found = find::find(&paths, dir.path()).await;
    served.await.unwrap();

    assert_eq!(found.folders, [folder]);
}

#[tokio::test]
async fn a_recorded_folder_whose_place_is_unknown_gets_no_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    koidrive_state_file::create_dir(&paths.state_dir, &paths.state_dir).unwrap();
    let folder = dir.path().join("Koidrive");
    fs::write(
        paths.state_dir.join(RECORD),
        serde_json::json!({ "folders": [&folder, "", "Elsewhere"], "owner": null }).to_string(),
    )
    .unwrap();

    let found = find::find(&paths, dir.path()).await;

    assert_eq!(found.folders, [folder]);
}
