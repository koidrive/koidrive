use super::*;
use crate::program::Arg;

async fn ask(paths: &Paths, found: &Found, request: Request) -> Result<Response, CliError> {
    match found.state {
        true => ipc::run(paths, request).await,
        false => match ipc::ask(paths, &request).await? {
            Some(response) => ipc::answered(response),
            None => Err(CliError::from("background sync did not answer".to_owned())),
        },
    }
}

pub(super) async fn unlink(paths: &Paths, found: &Found, run: &mut Run) -> SetAside {
    let mut all = SetAside::default();
    for folder in &found.folders {
        let request = Request::Unlink {
            folder: folder.clone(),
        };
        match ask(paths, found, request).await {
            Ok(Response::Unlinked { set_aside, .. }) => {
                all.brought_back.extend(set_aside.brought_back);
                all.kept.extend(set_aside.kept);
            }
            Ok(_) => {}
            Err(error) => run.problems.push(format!(
                "cannot unlink {}: {}",
                shown_folder(folder),
                error.message
            )),
        }
    }
    all
}

pub(super) async fn sign_out(
    paths: &Paths,
    found: &Found,
    options: &Options,
    terminal: &Terminal,
    run: &mut Run,
) -> (DeviceOutcome, Option<SetAside>) {
    let still_listed = |problem| (listed(found, problem), None);
    if !found.paired {
        let device = match koidrive_driver::Unconfirmed::recorded(paths) {
            Some(unconfirmed) => signed_out_unconfirmed(unconfirmed),
            None => DeviceOutcome::NotPaired,
        };
        return (device, None);
    }
    if options.keep_signed_in {
        return still_listed(None);
    }
    if !found.signs_out {
        return still_listed(Some(format!(
            "Koidrive has no record of creating {}, and no running background sync took the sign-out",
            shown_folder(&paths.state_dir)
        )));
    }
    let leave = |confirm_last_device| {
        ask(
            paths,
            found,
            Request::Leave {
                confirm_last_device,
            },
        )
    };
    let mut answer = leave(options.last_device).await;
    if let Err(error) = &answer
        && !options.last_device
        && !options.yes
        && terminal.guided()
        && error
            .next
            .iter()
            .any(|next| next.argv.iter().any(|arg| arg == "--last-device"))
    {
        terminal.message(&printable(&error.message));
        if terminal
            .confirm("Sign this device out anyway?", false)
            .await
            .unwrap_or(false)
        {
            answer = leave(true).await;
        }
    }
    match answer {
        Ok(Response::SignedOut {
            unconfirmed: Some(unconfirmed),
            set_aside,
            ..
        }) => (signed_out_unconfirmed(unconfirmed), Some(set_aside)),
        Ok(Response::SignedOut { set_aside, .. }) => match &found.owner {
            Some(owner) => (
                DeviceOutcome::SignedOut {
                    server: owner.server.clone(),
                    unconfirmed: None,
                },
                Some(set_aside),
            ),
            None => (
                listed(found, Some("the server's address is unknown".into())),
                Some(set_aside),
            ),
        },
        failed => {
            run.unrevoked = (found.owner.as_ref()).map(|owner| owner.device_id.clone());
            still_listed(Some(match failed {
                Ok(other) => format!("the daemon answered {other:?}"),
                Err(error) => error.message,
            }))
        }
    }
}

fn signed_out_unconfirmed(unconfirmed: koidrive_driver::Unconfirmed) -> DeviceOutcome {
    DeviceOutcome::SignedOut {
        server: unconfirmed.server.clone(),
        unconfirmed: Some(UnconfirmedData {
            server: unconfirmed.server,
            device_id: unconfirmed.device_id,
            reason: unconfirmed.reason,
        }),
    }
}

pub(super) async fn remove_service(found: &Found, run: &mut Run) {
    let Some((manager, service)) = &found.service else {
        return;
    };
    let name = manager.name(service);
    let removed = match manager.state(service) {
        Ok(State::NotInstalled) => Ok(()),
        Ok(_) => manager.remove(service),
        Err(problem) if manager.kind() == ServiceManager::Systemd => {
            remove_path(&service.file(manager.kind()))
                .map(drop)
                .map_err(|error| format!("{problem}; {error}"))
        }
        Err(problem) => Err(problem),
    };
    let mut removed = removed;
    let deadline = Instant::now() + RELEASE;
    while removed.is_ok() {
        match manager.state(service) {
            Ok(State::NotInstalled) => break,
            Err(_) if !service.file(manager.kind()).exists() => break,
            Ok(_) if Instant::now() >= deadline => {
                removed = Err("it is still loaded after removal".to_owned());
            }
            Ok(_) => tokio::time::sleep(Duration::from_millis(200)).await,
            Err(problem) => removed = Err(problem),
        }
    }
    match removed {
        Ok(()) => run.removed.push(Removed::Service { name }),
        Err(problem) => run.problems.push(format!(
            "cannot remove the background service {name}: {problem}"
        )),
    }
}

pub(super) async fn released(paths: &Paths) -> Result<Option<Flock>, String> {
    let lock = paths.lock();
    let deadline = Instant::now() + RELEASE;
    loop {
        let file = match fs::File::open(&lock) {
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(format!("cannot open {}: {error}", shown_folder(&lock))),
            Ok(file) => file,
        };
        match Flock::take(file) {
            Ok(Some(lock)) => return Ok(Some(lock)),
            Ok(None) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Ok(None) => {
                return Err(
                    "a koi daemon still uses this device's profile, so its state stays; stop it"
                        .into(),
                );
            }
            Err(error) => return Err(format!("cannot lock {}: {error}", shown_folder(&lock))),
        }
    }
}

pub(super) fn clean_folder(root: &Path, owner: Option<&Owner>, run: &mut Run) {
    if fs::symlink_metadata(root).is_ok_and(|meta| meta.is_dir()) {
        run.kept.push(Kept::Folder {
            path: root.display().to_string(),
        });
    }
    let private = root.join(".koidrive");
    match folder_data(&private, owner) {
        Ok(Cleaned::Nothing) => {}
        Ok(Cleaned::Removed) => run.removed.push(Removed::FolderData {
            path: private.display().to_string(),
        }),
        Ok(Cleaned::LinkedElsewhere) => run.kept.push(Kept::LinkedElsewhere {
            path: private.display().to_string(),
        }),
        Ok(Cleaned::Kept(paths)) => {
            for path in paths {
                let held = [koidrive_fs::PARKED_DIR, koidrive_fs::QUARANTINE_DIR]
                    .iter()
                    .any(|dir| path == private.join(dir))
                    && fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir());
                let listed = held.then(|| {
                    fs::read_dir(&path).and_then(|entries| {
                        entries
                            .map(|entry| Ok(entry?.path()))
                            .collect::<io::Result<Vec<PathBuf>>>()
                    })
                });
                let mut entries = match listed {
                    Some(Ok(entries)) => entries,
                    _ => {
                        run.kept.push(Kept::FolderData {
                            path: path.display().to_string(),
                        });
                        continue;
                    }
                };
                entries.sort();
                run.kept
                    .extend(entries.into_iter().map(|path| Kept::SetAside {
                        path: path.display().to_string(),
                    }));
            }
        }
        Err(error) => run.problems.push(format!(
            "cannot clean up {}: {error}",
            shown_folder(&private)
        )),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Cleaned {
    Nothing,
    Removed,
    LinkedElsewhere,
    Kept(Vec<PathBuf>),
}

pub(super) fn folder_data(private: &Path, owner: Option<&Owner>) -> io::Result<Cleaned> {
    match fs::symlink_metadata(private) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Cleaned::Nothing),
        Err(error) => return Err(error),
        Ok(meta) if !meta.is_dir() => return Ok(Cleaned::Kept(vec![private.to_path_buf()])),
        Ok(_) => {}
    }
    match fs::read(private.join("device.json")) {
        Ok(bytes) => {
            let linked = serde_json::from_slice::<Owner>(&bytes).ok();
            if linked.is_none() || linked.as_ref() != owner {
                return Ok(Cleaned::LinkedElsewhere);
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    for name in FOLDER_RECORDS {
        remove_path(&private.join(name))?;
    }
    for name in RECOVERY_RECORDS {
        let record = private.join(name);
        if fs::symlink_metadata(&record).is_ok_and(|meta| !meta.is_dir()) {
            match fs::remove_file(&record) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
        }
    }
    let trash = private.join("trash");
    if fs::symlink_metadata(&trash).is_err() || prune_empty(&trash)? {
        for index in ["trash.json", "trash.json.tmp"] {
            remove_path(&private.join(index))?;
        }
    }
    let mut kept = Vec::new();
    for entry in fs::read_dir(private)? {
        let path = entry?.path();
        if !prune_empty(&path)? {
            kept.push(path);
        }
    }
    kept.sort();
    if !kept.is_empty() {
        return Ok(Cleaned::Kept(kept));
    }
    fs::remove_dir(private)?;
    Ok(Cleaned::Removed)
}

fn prune_empty(path: &Path) -> io::Result<bool> {
    if !fs::symlink_metadata(path)?.is_dir() {
        return Ok(false);
    }
    let mut empty = true;
    for entry in fs::read_dir(path)? {
        empty &= prune_empty(&entry?.path())?;
    }
    if empty {
        fs::remove_dir(path)?;
    }
    Ok(empty)
}

pub(super) fn remove_path(path: &Path) -> io::Result<bool> {
    crate::lifeline::confine(path)?;
    let removed = match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
    };
    match removed {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        other => other.map(|()| true),
    }
}

pub(super) fn prune_runtime(paths: &Paths) {
    if paths.runtime_dir != paths.state_dir {
        let _ = fs::remove_dir(&paths.runtime_dir);
    }
}

pub(super) fn strip_path_line(profile: &Path) -> io::Result<bool> {
    crate::lifeline::confine(profile)?;
    let text = match fs::read_to_string(profile) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        other => other?,
    };
    let rest: Vec<&str> = text.lines().filter(|line| !is_path_line(line)).collect();
    if rest.len() == text.lines().count() {
        return Ok(false);
    }
    let rest = rest.join("\n");
    let created = rest.trim().is_empty()
        || (profile.file_name() == Some(OsStr::new(".bash_profile"))
            && rest.trim() == BASH_PRELUDE);
    if created && !fs::symlink_metadata(profile)?.is_symlink() {
        fs::remove_file(profile)?;
        return Ok(true);
    }
    let target = fs::canonicalize(profile)?;
    crate::lifeline::confine(&target)?;
    let staged = koidrive_state_file::AtomicFile::prepare(&target)?;
    staged.as_file().write_all(rest.as_bytes())?;
    if !rest.is_empty() && text.ends_with('\n') {
        staged.as_file().write_all(b"\n")?;
    }
    staged
        .as_file()
        .set_permissions(fs::metadata(&target)?.permissions())?;
    staged.finish(koidrive_state_file::Publish::Replace)?;
    Ok(true)
}

async fn helper(
    program: &Path,
    command: io::Result<crate::program::Tokio>,
    args: &[Arg],
    within: Duration,
) -> Result<(), String> {
    let mut command =
        command.map_err(|error| format!("cannot run {}: {error}", program.display()))?;
    let output = command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(within, output).await {
        Err(_) => Err(format!(
            "{} did not finish within {} seconds",
            program.display(),
            within.as_secs()
        )),
        Ok(Err(error)) => Err(format!("cannot run {}: {error}", program.display())),
        Ok(Ok(output)) if output.status.success() => Ok(()),
        Ok(Ok(output)) => Err(format!(
            "{} {} failed ({}): {}",
            program.display(),
            args.iter()
                .map(Arg::to_string)
                .collect::<Vec<_>>()
                .join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

fn bundles(programs: &[Program]) -> Vec<&Path> {
    programs
        .iter()
        .filter_map(|program| match program {
            Program::App(bundle) | Program::Elsewhere(bundle) => Some(bundle.as_path()),
            Program::Homebrew {
                app: Some(bundle), ..
            } => Some(bundle.as_path()),
            _ => None,
        })
        .collect()
}

pub(super) async fn quit_apps(programs: &[Program], run: &mut Run) {
    let bundles = bundles(programs);
    if bundles.is_empty() || !cfg!(target_os = "macos") {
        return;
    }
    #[cfg(target_os = "macos")]
    {
        let mut args: Vec<Arg> = ["-l", "JavaScript", "-e", crate::app_install::QUIESCE]
            .into_iter()
            .map(Arg::from)
            .collect();
        for bundle in bundles {
            match crate::program::Confined::new(bundle) {
                Ok(bundle) => args.push(bundle.into()),
                Err(error) => {
                    run.problems
                        .push(format!("cannot quit the Koidrive app: {error}"));
                    return;
                }
            }
        }
        if let Err(problem) = helper(
            Path::new("/usr/bin/osascript"),
            Ok(crate::program::system_async("/usr/bin/osascript")),
            &args,
            HELPER,
        )
        .await
        {
            run.problems
                .push(format!("cannot quit the Koidrive app: {problem}"));
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = run;
}

async fn unregister_login_item(bundle: &Path, run: &mut Run) {
    let app = bundle.join("Contents/MacOS/Koidrive");
    if !app.is_file() {
        return;
    }
    if let Err(problem) = helper(
        &app,
        crate::program::found_async(&app),
        &["--unregister-login-item".into()],
        HELPER,
    )
    .await
    {
        run.kept.push(Kept::LoginItem {
            path: bundle.display().to_string(),
            problem,
        });
    }
}

pub(super) async fn remove_programs(programs: &[Program], profiles: &[PathBuf], run: &mut Run) {
    for bundle in bundles(programs) {
        unregister_login_item(bundle, run).await;
    }
    run.kept.extend(kept_programs(programs));
    let (commands, others): (Vec<_>, Vec<_>) =
        (programs.iter()).partition(|program| matches!(program, Program::Command(_)));
    for program in others {
        remove_program(program, programs, run).await;
    }
    if !run.problems.is_empty() {
        return;
    }
    for profile in profiles {
        match strip_path_line(profile) {
            Ok(true) => run.removed.push(Removed::ShellProfile {
                path: profile.display().to_string(),
            }),
            Ok(false) => {}
            Err(error) => run.problems.push(format!(
                "cannot remove the PATH line from {}: {error}",
                shown_folder(profile)
            )),
        }
    }
    if !run.problems.is_empty() {
        return;
    }
    for program in commands {
        remove_program(program, programs, run).await;
    }
}

async fn remove_program(program: &Program, programs: &[Program], run: &mut Run) {
    match program {
        Program::Command(path) => {
            let target = fs::symlink_metadata(path)
                .is_ok_and(|meta| meta.is_symlink())
                .then(|| fs::canonicalize(path).ok())
                .flatten();
            run.remove(
                path,
                Removed::Command {
                    path: path.display().to_string(),
                },
            );
            if let Some(target) = target
                && !programs.contains(&Program::Command(target.clone()))
                && fs::symlink_metadata(&target).is_ok()
            {
                run.kept.push(Kept::LinkTarget {
                    path: target.display().to_string(),
                });
            }
        }
        Program::App(path) => run.remove(
            path,
            Removed::App {
                path: path.display().to_string(),
            },
        ),
        Program::Homebrew { brew, cask, .. } => {
            let argv = vec![
                brew_word(brew),
                "uninstall".to_owned(),
                if *cask { "--cask" } else { "--formula" }.to_owned(),
                "koidrive".to_owned(),
            ];
            let args: Vec<Arg> = argv[1..].iter().map(Arg::from).collect();
            match helper(brew, crate::program::found_async(brew), &args, HOMEBREW).await {
                Ok(()) => run.removed.push(Removed::Homebrew { argv }),
                Err(problem) => {
                    run.problems.push(problem);
                    run.next.push(argv);
                }
            }
        }
        Program::Elsewhere(_) | Program::Development(_) => {}
    }
}

pub(super) fn keep_shared(found: &Found, run: &mut Run) {
    let shared = |path: &Path| Kept::Shared {
        path: path.display().to_string(),
    };
    for program in &found.programs {
        run.kept.push(match program {
            Program::Command(path) | Program::App(path) => shared(path),
            Program::Homebrew { koi, app, .. } => shared(app.as_deref().unwrap_or(koi)),
            Program::Elsewhere(_) | Program::Development(_) => continue,
        });
    }
    run.kept.extend(kept_programs(&found.programs));
    run.kept
        .extend(found.app_files.iter().map(|path| shared(path)));
    run.kept
        .extend(found.profiles.iter().map(|path| Kept::PathLine {
            path: path.display().to_string(),
        }));
    let on_path = crate::update::on_path(OsStr::new("koi"), std::env::var_os("PATH").as_deref())
        .and_then(|koi| fs::canonicalize(koi).ok());
    for other in &found.others {
        let binary = (other.binary.as_deref())
            .map(PathBuf::from)
            .into_iter()
            .chain(found.programs.iter().filter_map(|program| match program {
                Program::Command(path) => Some(path.clone()),
                Program::App(bundle) => Some(bundle.join("Contents/Helpers/koi")),
                Program::Homebrew { koi, .. } => Some(koi.clone()),
                Program::Elsewhere(_) | Program::Development(_) => None,
            }))
            .find(|binary| is_koi(binary))
            .or_else(|| std::env::current_exe().ok());
        let koi = match binary {
            Some(binary) if on_path.is_none() || fs::canonicalize(&binary).ok() != on_path => {
                binary.display().to_string()
            }
            _ => "koi".to_owned(),
        };
        let mut argv = vec!["env".to_owned()];
        for (name, value) in &other.env {
            if value.is_none() {
                argv.extend(["-u".to_owned(), (*name).to_owned()]);
            }
        }
        for (name, value) in &other.env {
            if let Some(value) = value {
                argv.push(format!("{name}={value}"));
            }
        }
        argv.extend([koi, "uninstall".to_owned()]);
        run.next.push(argv.clone());
        run.kept.push(Kept::OtherProfile {
            service: other.name.clone(),
            argv,
        });
    }
}

fn brew_word(brew: &Path) -> String {
    let selected = crate::update::on_path(OsStr::new("brew"), std::env::var_os("PATH").as_deref())
        .and_then(|selected| fs::canonicalize(selected).ok());
    match selected.is_some() && selected == fs::canonicalize(brew).ok() {
        true => "brew".to_owned(),
        false => brew.display().to_string(),
    }
}
