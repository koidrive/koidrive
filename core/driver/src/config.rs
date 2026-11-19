// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use koidrive_net::DEFAULT_UPLOAD_CONCURRENCY;
use serde::Deserialize;

/// The profile's configuration file (`config.toml`). It is device-local: never synced, and it holds no secrets.
///
/// ```toml
/// [sync]
/// include = ["Projects/", "Notes/2026"]
/// exclude = ["*.iso", "build/"]
/// gitignore = false
///
/// [trash]
/// max_size_mb = 2048
///
/// [upload]
/// concurrency = 4
///
/// [service]
/// background = false
///
/// [updates]
/// check = false
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub sync: SyncRules,
    #[serde(default)]
    pub trash: LocalTrash,
    #[serde(default)]
    pub upload: Uploads,
    #[serde(default)]
    pub service: BackgroundService,
    #[serde(default)]
    pub updates: UpdateCheck,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpdateCheck {
    pub check: bool,
}

impl Default for UpdateCheck {
    fn default() -> Self {
        UpdateCheck { check: true }
    }
}

/// The background service that the koi command installs and starts while the profile links a folder. With
/// `background = false` it neither installs, starts nor removes it, and the person runs `koi _daemon` some other way.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BackgroundService {
    pub background: bool,
}

impl Default for BackgroundService {
    fn default() -> Self {
        BackgroundService { background: true }
    }
}

/// The local trash of every folder the profile links (design §5.2), which holds what sync effects displaced. An
/// entry leaves it after [`koidrive_fs::TRASH_RETENTION`], or earlier, oldest first, while the folder's trash holds
/// more than `max_size_mb` megabytes (2048 when unset).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalTrash {
    pub max_size_mb: u64,
}

impl Default for LocalTrash {
    fn default() -> Self {
        LocalTrash { max_size_mb: 2048 }
    }
}

impl LocalTrash {
    pub fn cap(&self) -> u64 {
        self.max_size_mb.saturating_mul(1_000_000)
    }
}

/// How the profile uploads chunks. Every folder the profile links shares the limit, so all of them together have at
/// most `concurrency` full chunks of content in flight.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Uploads {
    pub concurrency: UploadConcurrency,
}

/// How many full 4 MiB chunks of content may be in flight at once, from 1 to 16. Smaller chunks share that room, so
/// many small files go at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(try_from = "i64")]
pub struct UploadConcurrency(usize);

impl UploadConcurrency {
    pub const MAX: usize = 16;

    pub fn get(self) -> usize {
        self.0
    }
}

impl Default for UploadConcurrency {
    fn default() -> Self {
        UploadConcurrency(DEFAULT_UPLOAD_CONCURRENCY)
    }
}

impl TryFrom<i64> for UploadConcurrency {
    type Error = String;

    fn try_from(value: i64) -> Result<Self, String> {
        match usize::try_from(value) {
            Ok(concurrency @ 1..=Self::MAX) => Ok(UploadConcurrency(concurrency)),
            _ => Err(format!(
                "concurrency must be from 1 to {}, not {value}",
                Self::MAX
            )),
        }
    }
}

/// This device's own selective sync, applied to every folder the profile links. It saves
/// bandwidth and disk, not access: the server still sends this device every name in the space, and its credential
/// can still read everything.
///
/// - `include`: paths relative to the folder's root. When the list is not empty, only these paths and what is under
///   them are synced.
/// - `exclude`: patterns in `.koiignore` syntax (`.gitignore` syntax, design §5.10), relative to the folder's root.
/// - `gitignore`: whether the folder's `.gitignore` files leave paths out along with its `.koiignore` files. True
///   unless set to false.
///
/// `include` and `exclude` turn into the same matcher input as a `.koiignore` file (see [`SyncRules::lines`]).
/// All three follow the same rule: becoming ignored never deletes anything, on either side.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SyncRules {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub gitignore: bool,
}

impl Default for SyncRules {
    fn default() -> Self {
        SyncRules {
            include: Vec::new(),
            exclude: Vec::new(),
            gitignore: true,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {}: {source}", koidrive_fs::quoted(.path))]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{} is not valid: {reason}", koidrive_fs::quoted(.path))]
    Invalid { path: PathBuf, reason: String },
}

impl Config {
    /// Reads the file, or returns the default configuration when there is none. Unknown keys and rules that cannot
    /// be turned into matcher input are refused, so a typo never silently syncs more than intended.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(source) => {
                return Err(ConfigError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        Config::parse(path, &text)
    }

    pub(crate) fn parse(path: &Path, text: &str) -> Result<Self, ConfigError> {
        let invalid = |reason: String| ConfigError::Invalid {
            path: path.to_path_buf(),
            reason,
        };
        let config: Config = toml::from_str(text).map_err(|err| invalid(err.to_string()))?;
        config.sync.lines().map_err(invalid)?;
        Ok(config)
    }
}

#[derive(Default)]
struct Included {
    whole: bool,
    children: BTreeMap<String, Included>,
}

impl SyncRules {
    pub fn lines(&self) -> Result<Vec<String>, String> {
        let mut lines = Vec::new();
        if !self.include.is_empty() {
            let mut root = Included::default();
            for path in &self.include {
                let mut node = &mut root;
                for part in include_parts(path)? {
                    node = node.children.entry(part.to_owned()).or_default();
                }
                node.whole = true;
            }
            lines.push("/*".to_owned());
            reinclude(&root, "", &mut lines);
        }
        for pattern in &self.exclude {
            if pattern.trim().is_empty() || pattern.contains(['\n', '\r']) {
                return Err(format!(
                    "exclude pattern {pattern:?} is empty or spans lines"
                ));
            }
            if pattern.starts_with('!') {
                return Err(format!(
                    "exclude pattern {pattern:?} starts with '!'; list the path under include instead"
                ));
            }
            if pattern.starts_with('#') {
                return Err(format!(
                    "exclude pattern {pattern:?} starts with '#', which makes it a comment; write \\# for a literal '#'"
                ));
            }
            lines.push(pattern.clone());
        }
        Ok(lines)
    }
}

fn include_parts(path: &str) -> Result<Vec<&str>, String> {
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    if parts.iter().any(|part| matches!(*part, "" | "." | "..")) || path.contains(['\n', '\r']) {
        return Err(format!(
            "include path {path:?} must be a path inside the folder, such as \"Projects/\""
        ));
    }
    Ok(parts)
}

fn reinclude(node: &Included, prefix: &str, lines: &mut Vec<String>) {
    for (name, child) in &node.children {
        let path = format!("{prefix}/{}", escape(name));
        if child.whole {
            lines.push(format!("!{path}"));
        } else {
            lines.push(format!("!{path}/"));
            lines.push(format!("{path}/*"));
            reinclude(child, &path, lines);
        }
    }
}

fn escape(name: &str) -> String {
    let mut escaped = String::with_capacity(name.len());
    for c in name.chars() {
        if matches!(c, '*' | '?' | '[' | '\\') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    if escaped.ends_with(' ') {
        escaped.insert(escaped.len() - 1, '\\');
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(include: &[&str], exclude: &[&str]) -> SyncRules {
        SyncRules {
            include: include.iter().map(|s| s.to_string()).collect(),
            exclude: exclude.iter().map(|s| s.to_string()).collect(),
            ..SyncRules::default()
        }
    }

    #[test]
    fn no_rules_mean_no_lines() {
        assert!(SyncRules::default().lines().unwrap().is_empty());
    }

    #[test]
    fn includes_reinclude_each_path_and_its_parents() {
        let lines = rules(
            &["Projects/", "Docs/a/b", "/notes.txt", "Docs/a/b/c"],
            &["*.iso"],
        )
        .lines()
        .unwrap();
        assert_eq!(
            lines,
            [
                "/*",
                "!/Docs/",
                "/Docs/*",
                "!/Docs/a/",
                "/Docs/a/*",
                "!/Docs/a/b",
                "!/Projects",
                "!/notes.txt",
                "*.iso",
            ]
        );
    }

    #[test]
    fn a_whole_folder_covers_paths_listed_inside_it() {
        assert_eq!(rules(&["a/b", "a"], &[]).lines().unwrap(), ["/*", "!/a"]);
    }

    #[test]
    fn included_names_are_matched_literally() {
        assert_eq!(
            rules(&["[draft] *?\\x "], &[]).lines().unwrap(),
            ["/*", "!/\\[draft] \\*\\?\\\\x\\ "]
        );
    }

    #[test]
    fn paths_outside_the_folder_and_negated_excludes_are_refused() {
        for bad in ["", "/", "../x", "a/../b", "a//b", "./a"] {
            assert!(rules(&[bad], &[]).lines().is_err(), "{bad:?}");
        }
        for bad in ["", "  ", "!keep", "#backup", "a\nb"] {
            assert!(rules(&[], &[bad]).lines().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_file_is_parsed_strictly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(Config::load(&path).unwrap(), Config::default());

        std::fs::write(
            &path,
            "[sync]\ninclude = [\"Projects/\"]\nexclude = [\"*.tmp\"]\n",
        )
        .unwrap();
        assert_eq!(
            Config::load(&path).unwrap().sync,
            rules(&["Projects/"], &["*.tmp"])
        );

        std::fs::write(&path, "[sync]\ninclude = [\"Projects/\"]\nexlude = []\n").unwrap();
        assert!(matches!(
            Config::load(&path),
            Err(ConfigError::Invalid { .. })
        ));

        std::fs::write(&path, "[sync]\ninclude = [\"../outside\"]\n").unwrap();
        assert!(matches!(
            Config::load(&path),
            Err(ConfigError::Invalid { .. })
        ));

        assert_eq!(Config::default().trash.cap(), 2_048_000_000);
        assert!(Config::default().service.background);
        std::fs::write(&path, "[service]\nbackground = false\n").unwrap();
        assert!(!Config::load(&path).unwrap().service.background);
        std::fs::write(&path, "[service]\nbackgrund = false\n").unwrap();
        assert!(matches!(
            Config::load(&path),
            Err(ConfigError::Invalid { .. })
        ));
        std::fs::write(&path, "[trash]\nmax_size_mb = 10\n").unwrap();
        assert_eq!(Config::load(&path).unwrap().trash.cap(), 10_000_000);
        std::fs::write(&path, "[trash]\nmax_size_gb = 10\n").unwrap();
        assert!(matches!(
            Config::load(&path),
            Err(ConfigError::Invalid { .. })
        ));
    }

    #[test]
    fn gitignore_files_are_followed_unless_the_sync_table_turns_them_off() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let load = |text: &str| {
            std::fs::write(&path, text).unwrap();
            Config::load(&path).map(|config| config.sync.gitignore)
        };
        assert!(Config::load(&path).unwrap().sync.gitignore);
        assert!(load("").unwrap());
        assert!(load("[sync]\n").unwrap());
        assert!(load("[sync]\nexclude = [\"*.tmp\"]\n").unwrap());
        assert!(load("[sync]\ngitignore = true\n").unwrap());
        assert!(!load("[sync]\ngitignore = false\n").unwrap());
        assert!(matches!(
            load("[sync]\ngitignore = \"no\"\n"),
            Err(ConfigError::Invalid { .. })
        ));
    }

    #[test]
    fn upload_concurrency_is_read_from_1_to_16_and_defaults_to_4() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let load = |text: &str| {
            std::fs::write(&path, text).unwrap();
            Config::load(&path).map(|config| config.upload.concurrency.get())
        };
        let refused = |text: &str| match load(text) {
            Err(ConfigError::Invalid { reason, .. }) => reason,
            other => panic!("{text:?} was not refused: {other:?}"),
        };

        assert_eq!(load("").unwrap(), 4);
        assert_eq!(load("[upload]\n").unwrap(), 4);
        assert_eq!(load("[upload]\nconcurrency = 1\n").unwrap(), 1);
        assert_eq!(load("[upload]\nconcurrency = 16\n").unwrap(), 16);
        for bad in [0, 17, -1] {
            let reason = refused(&format!("[upload]\nconcurrency = {bad}\n"));
            assert!(
                reason.contains(&format!("concurrency must be from 1 to 16, not {bad}")),
                "{reason}"
            );
        }
        let reason = refused("[upload]\nconcurrency = \"8\"\n");
        assert!(reason.contains("invalid type"), "{reason}");
        let reason = refused("[upload]\nconcurrency = 2.5\n");
        assert!(reason.contains("invalid type"), "{reason}");
        let reason = refused("[upload]\nconcurency = 8\n");
        assert!(reason.contains("unknown field"), "{reason}");
    }

    #[test]
    fn the_update_check_is_on_unless_the_updates_table_turns_it_off() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let load = |text: &str| {
            std::fs::write(&path, text).unwrap();
            Config::load(&path).map(|config| config.updates.check)
        };
        assert!(load("").unwrap());
        assert!(load("[updates]\n").unwrap());
        assert!(!load("[updates]\ncheck = false\n").unwrap());
        assert!(matches!(
            load("[updates]\nchecks = false\n"),
            Err(ConfigError::Invalid { .. })
        ));
        assert!(matches!(
            load("[updates]\ncheck = \"no\"\n"),
            Err(ConfigError::Invalid { .. })
        ));
    }
}
