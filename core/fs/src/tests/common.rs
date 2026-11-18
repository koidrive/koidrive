// SPDX-License-Identifier: Apache-2.0

use std::fs;

use notify::Watcher as _;

use notify::event::Flag;

use super::*;

fn blocks_reads<T: std::fmt::Debug>(test: &str, probe: std::io::Result<T>) -> bool {
    use std::io::Write;

    match probe {
        Err(err) => {
            assert_eq!(
                err.kind(),
                std::io::ErrorKind::PermissionDenied,
                "{test}: {err}"
            );
            true
        }
        Ok(found) => {
            assert!(
                rustix::process::geteuid().is_root(),
                "{test}: the permission change did not block reads ({found:?})"
            );
            let _ = writeln!(
                std::io::stderr(),
                "SKIPPED {test}: running as root, so permissions do not block reads"
            );
            false
        }
    }
}

fn tree(root: &Path, files: usize) {
    fs::create_dir_all(root.join("d")).unwrap();
    fs::write(root.join("d/x"), "x").unwrap();
    for i in 0..files {
        fs::write(root.join(format!("f{i}.txt")), "f").unwrap();
    }
}

fn created(path: &str, kind: EntryKind) -> Change {
    Change::Created {
        path: path.into(),
        kind,
    }
}

fn root_id(root: &Path) -> FileId {
    stat_fd(open_root(root).unwrap().as_fd()).unwrap().id
}

#[derive(Clone, Default)]
struct Lines(std::sync::Arc<std::sync::Mutex<BTreeMap<PathBuf, Vec<u8>>>>);

impl Ignore for Lines {
    fn set_rules(&mut self, dir: &Path, rules: Option<&[u8]>) {
        let mut given = self.0.lock().unwrap();
        match rules {
            Some(rules) => given.insert(dir.to_path_buf(), rules.to_vec()),
            None => given.remove(dir),
        };
    }

    fn status(&self, path: &Path, _: EntryKind) -> Ignored {
        let given = self.0.lock().unwrap();
        let mut dir = PathBuf::new();
        for name in path {
            let left_out = given.get(&dir).is_some_and(|rules| {
                rules
                    .split(|byte| *byte == b'\n')
                    .any(|line| line == name.as_bytes())
            });
            if left_out {
                return Ignored::Rule;
            }
            dir.push(name);
        }
        Ignored::No
    }

    fn key(&self, name: &OsStr) -> OsString {
        name.to_os_string()
    }
}
