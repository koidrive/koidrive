use super::*;

pub(super) async fn find(paths: &Paths, home: &Path) -> Found {
    let state = is_state(&paths.state_dir);
    let unrecorded = !koidrive_state_file::made(&paths.state_dir)
        && fs::metadata(&paths.state_dir).is_ok_and(|meta| meta.is_dir())
        && fs::symlink_metadata(paths.state_dir.join(".koidrive")).is_err();
    let recorded = match state {
        true => read_record(paths),
        false => Record::default(),
    };
    let credential = paths.credential();
    let paired = credential.exists();
    let signs_out = paired
        && (state
            || matches!(
                tokio::time::timeout(STATUS, ipc::ask(paths, &Request::Status)).await,
                Ok(Ok(Some(_)))
            ));
    let owner = Credential::load(&credential)
        .ok()
        .flatten()
        .map(|credential| Owner {
            server: credential.server,
            device_id: credential.device_id,
        })
        .or(recorded.owner);
    let mut folders = recorded.folders;
    if state
        && let Ok(Ok(Response::Status { status })) =
            tokio::time::timeout(STATUS, ipc::run(paths, Request::Status)).await
    {
        for folder in status.folders {
            let folder = PathBuf::from(folder.path);
            if !folders.contains(&folder) {
                folders.push(folder);
            }
        }
    }
    let manager = service::system();
    let this = service::this_service(paths).ok();
    let own = this.as_ref().map(|service| manager.name(service));
    let (others, unlisted) = match service::installed_services(manager.kind(), home) {
        Ok(installed) => (
            (installed.into_iter())
                .filter(|installed| Some(&installed.name) != own.as_ref())
                .collect::<Vec<_>>(),
            None,
        ),
        Err(problem) => (Vec::new(), Some(problem)),
    };
    let mut files = Vec::new();
    if cfg!(target_os = "macos")
        && let Some(service) = &this
    {
        files.push(service.log_file());
    }
    let service = this.filter(|service| match manager.state(service) {
        Ok(state) => state != State::NotInstalled,
        Err(_) => service.file(manager.kind()).exists(),
    });
    let mut also = Vec::new();
    if let Some(service) = &service
        && let Some(binary) = manager.installed(service)
    {
        also.push(PathBuf::from(binary));
    }
    let programs = programs(home, &also);
    let mut kept = Vec::new();
    match state || dangling(&paths.state_dir) {
        true => files.push(paths.state_dir.clone()),
        false if unrecorded => kept.push(Kept::UnrecordedState {
            path: paths.state_dir.display().to_string(),
        }),
        false if fs::symlink_metadata(&paths.state_dir).is_ok() => kept.push(Kept::Unrecognized {
            path: paths.state_dir.display().to_string(),
        }),
        false => {}
    }
    let config = paths.config_file.parent().map(Path::to_path_buf);
    let config_shared =
        unlisted.is_some() || others.iter().any(|other| uses_config(other, home, paths));
    let config_in_state = !config_shared
        && config
            .as_deref()
            .is_some_and(|config| same_dir(config, &paths.state_dir));
    match config {
        Some(config) if config_shared => {
            if fs::symlink_metadata(&config).is_ok() {
                kept.push(Kept::Shared {
                    path: config.display().to_string(),
                });
            }
        }
        Some(config) if !config_in_state => files.push(config),
        _ => {}
    }
    if paths.runtime_dir != paths.state_dir {
        files.push(paths.socket());
    }
    let mut app_files = Vec::new();
    if cfg!(target_os = "macos") {
        app_files.extend(
            [
                "Library/Logs/Koidrive",
                "Library/Application Support/Koidrive",
                "Library/Preferences/dev.koidrive.Koidrive.plist",
                "Library/Caches/dev.koidrive.Koidrive",
                "Library/Caches/dev.koidrive.app-install",
                "Library/HTTPStorages/dev.koidrive.Koidrive",
                "Library/Saved Application State/dev.koidrive.Koidrive.savedState",
            ]
            .iter()
            .map(|path| home.join(path)),
        );
        app_files.extend(
            application_folders(home).map(|folder| folder.join(".koidrive-app-install.lock")),
        );
    }
    files.retain(|path| fs::symlink_metadata(path).is_ok());
    app_files.retain(|path| fs::symlink_metadata(path).is_ok());
    Found {
        paired,
        signs_out,
        state,
        owner,
        folders,
        service: service.map(|service| (manager, service)),
        others,
        programs,
        files,
        app_files,
        profiles: profiles(home)
            .into_iter()
            .filter(|path| has_path_line(path))
            .collect(),
        kept,
        unlisted,
        config_in_state,
    }
}

fn uses_config(other: &service::Installed, home: &Path, paths: &Paths) -> bool {
    Paths::from_vars(|name| match name {
        "HOME" => Some(home.into()),
        _ => (other.env.iter())
            .find(|(key, _)| *key == name)
            .and_then(|(_, value)| value.clone())
            .map(Into::into),
    })
    .is_ok_and(
        |theirs| match (theirs.config_file.parent(), paths.config_file.parent()) {
            (Some(theirs), Some(ours)) => same_dir(theirs, ours),
            _ => false,
        },
    )
}

fn same_dir(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => a == b,
    }
}

fn application_folders(home: &Path) -> impl Iterator<Item = PathBuf> {
    #[cfg(feature = "test-install-inventory")]
    if let Some(root) = std::env::var_os("KOIDRIVE_TEST_INSTALL_INVENTORY") {
        return vec![PathBuf::from(root).join("Applications")].into_iter();
    }
    vec![PathBuf::from("/Applications"), home.join("Applications")].into_iter()
}

fn programs(home: &Path, also: &[PathBuf]) -> Vec<Program> {
    let mut programs = Vec::new();
    for install in Install::every(also) {
        let binary = install.binary().to_path_buf();
        let bundle = || binary.ancestors().nth(3).map(Path::to_path_buf);
        let program = match install.method() {
            Method::Development => Program::Development(binary.clone()),
            Method::AppDownload => Program::Elsewhere(bundle().unwrap_or_else(|| binary.clone())),
            _ if !is_koi(&binary) => continue,
            Method::Installer => {
                for location in command_locations(home, also, &binary) {
                    let program = Program::Command(location);
                    if !programs.contains(&program) {
                        programs.push(program);
                    }
                }
                continue;
            }
            Method::App => match bundle() {
                Some(bundle) => Program::App(bundle),
                None => continue,
            },
            Method::Cask { brew } | Method::CaskElsewhere { brew } => Program::Homebrew {
                brew: brew.clone(),
                cask: true,
                koi: binary.clone(),
                app: bundle(),
            },
            Method::Formula { brew } => Program::Homebrew {
                brew: brew.clone(),
                cask: false,
                koi: binary.clone(),
                app: None,
            },
        };
        if !programs.contains(&program) {
            programs.push(program);
        }
    }
    programs
}

fn command_locations(home: &Path, also: &[PathBuf], binary: &Path) -> Vec<PathBuf> {
    let invoked = std::env::args_os()
        .next()
        .map(PathBuf::from)
        .filter(|arg| arg.components().count() > 1)
        .and_then(|arg| std::path::absolute(arg).ok());
    let on_path = crate::update::on_path(OsStr::new("koi"), std::env::var_os("PATH").as_deref());
    let mut locations: Vec<PathBuf> = Vec::new();
    for location in std::iter::once(home.join(".local/bin/koi"))
        .chain(invoked)
        .chain(on_path)
        .chain(also.iter().cloned())
    {
        if fs::canonicalize(&location).is_ok_and(|resolved| resolved == binary)
            && !locations.contains(&location)
        {
            locations.push(location);
        }
    }
    if locations.is_empty() {
        locations.push(binary.to_path_buf());
    }
    locations
}

pub(super) fn is_koi(binary: &Path) -> bool {
    fs::read(binary).is_ok_and(|bytes| {
        bytes
            .windows(KOI_MARK.len())
            .any(|window| window == KOI_MARK)
    })
}

fn profiles(home: &Path) -> Vec<PathBuf> {
    let absolute = |name| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    let mut profiles: Vec<PathBuf> = absolute("ZDOTDIR")
        .map(|dir| dir.join(".zprofile"))
        .into_iter()
        .collect();
    profiles.push(home.join(".zprofile"));
    profiles.push(home.join(".bash_profile"));
    profiles.push(
        absolute("XDG_CONFIG_HOME")
            .unwrap_or_else(|| home.join(".config"))
            .join("fish/conf.d/koidrive.fish"),
    );
    profiles.dedup();
    profiles
}

pub(super) fn is_path_line(line: &str) -> bool {
    PATH_MARKERS.iter().any(|marker| line.ends_with(marker))
}

fn has_path_line(profile: &Path) -> bool {
    fs::read_to_string(profile).is_ok_and(|text| text.lines().any(is_path_line))
}

pub(super) fn kept_programs(programs: &[Program]) -> Vec<Kept> {
    programs
        .iter()
        .filter_map(|program| match program {
            Program::Development(path) => Some(Kept::DevelopmentBuild {
                path: path.display().to_string(),
            }),
            Program::Elsewhere(path) => Some(Kept::App {
                path: path.display().to_string(),
            }),
            _ => None,
        })
        .collect()
}
