// SPDX-License-Identifier: Apache-2.0

use std::os::unix::fs::PermissionsExt;

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
         https://koidrive.ada.workers.dev still lists this device. To remove it, run this on another device: koi device revoke d1\n\
         Removed the background service dev.koidrive.koi.\n\
         Kept /srv/Koidrive and every file in it.\n\
         Kept /srv/Koidrive/.koidrive/trash, which still holds files. Delete it when you no longer need them.\n"
    );
}

#[test]
fn only_what_koidrive_writes_leaves_a_state_folder() {
    assert!(Paths::STATE_ENTRIES.contains(&RECORD));
    for name in [
        "credential.json",
        "credential.next",
        ".credential.next.123.tmp",
        ".credential.json.9.tmp",
        "leaving.next",
        ".tmpA1b2C3",
    ] {
        assert!(is_state_entry(name), "{name}");
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
        assert!(!is_state_entry(name), "{name}");
    }
}
