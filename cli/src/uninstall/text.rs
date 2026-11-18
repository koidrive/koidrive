use super::*;

pub(super) fn finish(device: DeviceOutcome, run: Run) -> Result<Done, CliError> {
    let Run {
        removed,
        brought_back,
        kept,
        problems,
        next,
        unrevoked,
        ..
    } = run;
    let data = UninstalledData {
        device,
        removed,
        brought_back,
        kept,
    };
    let other = (data.kept.iter()).any(|kept| matches!(kept, Kept::OtherProfile { .. }));
    let header = match (other, data.removed.is_empty()) {
        (false, _) => {
            "Removed Koidrive from this computer.
"
        }
        (true, false) => {
            "Removed this Koidrive profile from this computer.
"
        }
        (true, true) => {
            "Nothing of this Koidrive profile is left to remove.
"
        }
    };
    let text = text(&data);
    let mut done = Done::ok(data, text);
    for argv in &next {
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        done = done.then(&argv);
    }
    let message = match (problems.is_empty(), next.is_empty()) {
        (false, true) => format!("{}; run koi uninstall again to finish", problems.join("; ")),
        (false, false) => problems.join("; "),
        (true, _) => {
            done.text.insert_str(0, header);
            let Some(device_id) = unrevoked else {
                done.tone = Tone::Completed;
                return Ok(done);
            };
            format!(
                "the server may still list this device; to remove it, run koi device revoke {} on another device",
                crate::output::command_word(&device_id)
            )
        }
    };
    Err(CliError {
        next: done.next.clone(),
        partial: Some(Box::new(done)),
        ..CliError::new(koidrive_ipc::Code::Failed, message)
    })
}

pub(super) fn text(data: &UninstalledData) -> String {
    let mut lines = Vec::new();
    match &data.device {
        DeviceOutcome::NotPaired => {}
        DeviceOutcome::SignedOut {
            server,
            unconfirmed: None,
        } => {
            lines.push(format!("Signed this device out of {}.", printable(server)));
        }
        DeviceOutcome::SignedOut {
            server,
            unconfirmed: Some(unconfirmed),
        } => {
            lines.push(format!(
                "Signed this device out, but {} did not confirm that it revoked it: {}.",
                printable(server),
                printable(&unconfirmed.reason)
            ));
            lines.push(format!(
                "If it still lists this device, run this on another device signed in to {}: koi device revoke {}",
                printable(server),
                crate::output::command_word(&unconfirmed.device_id)
            ));
            lines.push("If you have no other device, add one with koi server add-device --interactive and run the command there.".to_owned());
        }
        DeviceOutcome::StillListed {
            server,
            device_id,
            problem: Some(problem),
        } => {
            lines.push(format!(
                "Could not sign this device out: {}.",
                printable(problem)
            ));
            let (listing, signed_in) = match server {
                Some(server) => (printable(server), printable(server)),
                None => ("Its server".to_owned(), "that server".to_owned()),
            };
            lines.push(match device_id {
                Some(id) => format!(
                    "{listing} may still list this device. To remove it, run this on another device signed in to {signed_in}: koi device revoke {}",
                    crate::output::command_word(id)
                ),
                None => format!(
                    "{listing} may still list this device. To remove it, revoke it from another device signed in to {signed_in} with koi device revoke."
                ),
            });
            lines.push("If you have no other device, add one with koi server add-device --interactive and run the command there.".to_owned());
        }
        DeviceOutcome::StillListed {
            server,
            device_id,
            problem: None,
        } => {
            let server = server
                .as_deref()
                .map_or_else(|| "Its server".to_owned(), printable);
            lines.push(match device_id {
                Some(id) => format!(
                    "{server} still lists this device. To remove it, run this on another device: koi device revoke {}",
                    crate::output::command_word(id)
                ),
                None => format!(
                    "{server} still lists this device. To remove it, revoke it from another device with koi device revoke."
                ),
            });
        }
    }
    for removed in &data.removed {
        let shown = |path: &str| crate::output::path(Path::new(path));
        lines.push(match removed {
            Removed::Service { name } => format!("Removed the background service {name}."),
            Removed::App { path } => format!("Removed the app {}.", shown(path)),
            Removed::Command { path } | Removed::Files { path } | Removed::FolderData { path } => {
                format!("Removed {}.", shown(path))
            }
            Removed::Homebrew { argv } => format!("Ran {}.", argv.join(" ")),
            Removed::ShellProfile { path } => {
                format!("Removed the PATH line from {}.", shown(path))
            }
        });
    }
    for path in &data.brought_back {
        lines.push(format!(
            "Brought back {}, which Koidrive had set aside while syncing.",
            crate::output::path(Path::new(path))
        ));
    }
    let mut text = lines.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(&kept_text(&data.kept));
    text
}

pub(super) fn kept_text(kept: &[Kept]) -> String {
    let mut text = String::new();
    for kept in kept {
        let shown = |path: &str| crate::output::path(Path::new(path));
        let line = match kept {
            Kept::Folder { path } => format!("Kept {} and every file in it.", shown(path)),
            Kept::FolderData { path } => format!(
                "Kept {}, which still holds files. Delete it when you no longer need them.",
                shown(path)
            ),
            Kept::SetAside { path } => format!(
                "Could not bring back {}, which Koidrive had set aside while syncing. Move it out of .koidrive to keep it.",
                shown(path)
            ),
            Kept::LinkedElsewhere { path } => format!(
                "Kept {}, because another device or profile links its folder.",
                shown(path)
            ),
            Kept::DevelopmentBuild { path } => {
                format!("Kept the development build {}.", shown(path))
            }
            Kept::App { path } => format!(
                "Kept {}, which is outside the Applications folders. Move it to the Trash.",
                shown(path)
            ),
            Kept::InFolder { path, folder } => format!(
                "Kept {}, because it holds or is inside the synced folder {}.",
                shown(path),
                shown(folder)
            ),
            Kept::Unrecognized { path } => format!(
                "Kept {}, which holds files Koidrive does not write.",
                shown(path)
            ),
            Kept::UnrecordedState { path } => format!(
                "Kept {}, because Koidrive has no record of creating it. If it holds only Koidrive's files, delete it with: rm -rf {}",
                shown(path),
                crate::pairing::shell_word(path)
            ),
            Kept::LinkTarget { path } => {
                format!("Kept {}, which the koi command linked to.", shown(path))
            }
            Kept::Shared { path } => format!(
                "Kept {}, which another Koidrive profile on this computer uses.",
                shown(path)
            ),
            Kept::PathLine { path } => format!(
                "Kept the PATH line in {}, which another Koidrive profile on this computer uses.",
                shown(path)
            ),
            Kept::OtherProfile { service, argv } => format!(
                "Another Koidrive profile runs the background service {service}. To remove it too, run: {}",
                argv.iter()
                    .map(|word| crate::pairing::shell_word(word))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            Kept::LoginItem { path, problem } => format!(
                "Could not remove {} from Login Items ({}). Remove it in System Settings > General > Login Items.",
                shown(path),
                printable(problem)
            ),
        };
        text.push_str(&line);
        text.push('\n');
    }
    text
}
