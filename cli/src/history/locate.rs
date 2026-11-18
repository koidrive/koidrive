use super::*;

/// The folders the profile links, from the daemon, or from the profile's stores read without writing anything when no
/// daemon runs.
pub(super) async fn linked_folders(paths: &Paths) -> Result<Vec<FolderStatus>, CliError> {
    let reached = match ipc::ask(paths, &Request::Status).await? {
        Some(response) => ipc::Reached::Daemon(Box::new(response)),
        None => {
            ipc::reach(paths, &Request::Status, ipc::STARTING, || {
                Driver::linked_folders(paths)
            })
            .await?
        }
    };
    match reached {
        ipc::Reached::Daemon(response) => match *response {
            Response::Status { status } => Ok(status.folders),
            other => Err(unexpected(other)),
        },
        ipc::Reached::Profile(folders) => Ok(folders),
    }
}

pub(super) async fn locate(
    paths: &Paths,
    path: &Path,
) -> Result<(FolderStatus, Vec<String>), CliError> {
    let folders = linked_folders(paths).await?;
    let typed = lexical(&crate::absolute(path)?);
    real_path(&typed)
        .and_then(|real| {
            folders
                .iter()
                .flat_map(|folder| {
                    let stored = PathBuf::from(&folder.path);
                    let resolved = real_path(&stored);
                    [Some(stored), resolved]
                        .into_iter()
                        .flatten()
                        .filter_map(|root| {
                            Some((folder, root.as_os_str().len(), below(&root, &real)?))
                        })
                        .collect::<Vec<_>>()
                })
                .max_by_key(|(_, length, _)| *length)
                .map(|(folder, _, names)| (folder.clone(), names))
        })
        .ok_or_else(|| {
            CliError::usage(format!(
                "{} is not in a linked folder; koi status lists them",
                printable(&typed.display().to_string())
            ))
            .because(Reason::NotInLinkedFolder)
        })
}

fn real_path(path: &Path) -> Option<PathBuf> {
    let mut path = path.to_path_buf();
    for _ in 0..=32 {
        let (existing, rest) = deepest_existing(&path);
        let Some((name, rest)) = rest.split_first() else {
            return Some(existing);
        };
        let next = match std::fs::read_link(existing.join(name)) {
            Ok(destination) => existing.join(destination),
            Err(_) => {
                return Some(
                    rest.iter()
                        .fold(existing.join(name), |path, name| path.join(name)),
                );
            }
        };
        path = rest.iter().fold(next, |path, name| path.join(name));
    }
    None
}

fn deepest_existing(path: &Path) -> (PathBuf, Vec<std::ffi::OsString>) {
    let mut existing = path;
    let mut rest = Vec::new();
    loop {
        if let Ok(real) = std::fs::canonicalize(existing) {
            rest.reverse();
            return (real, rest);
        }
        let (Some(parent), Some(name)) = (existing.parent(), existing.file_name()) else {
            rest.reverse();
            return (existing.to_path_buf(), rest);
        };
        rest.push(name.to_os_string());
        existing = parent;
    }
}

/// `path` without `.` names, and with each `..` removing the name before it, worked out without reading the disk.
pub(super) fn lexical(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normal.pop();
            }
            other => normal.push(other),
        }
    }
    normal
}

/// The names of `path` below `root`, or `None` when it is not inside it.
pub(super) fn below(root: &Path, path: &Path) -> Option<Vec<String>> {
    path.strip_prefix(root)
        .ok()?
        .components()
        .map(|component| match component {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

/// Whether `names` begins with `prefix`, comparing names as the server does.
pub(super) fn starts_with(names: &[String], prefix: &[String]) -> bool {
    names.len() >= prefix.len()
        && names
            .iter()
            .zip(prefix)
            .all(|(name, wanted)| name_key(name) == name_key(wanted))
}
