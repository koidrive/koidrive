// SPDX-License-Identifier: Apache-2.0

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;

use koidrive_cloudflare::Jurisdiction;
use koidrive_driver::Paths;
use serde::{Deserialize, Serialize};

use crate::error::CliError;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    #[default]
    Planned,
    BucketCreated,
    ScriptUploaded,
    Serving,
    Uninstalling,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupState {
    pub account: String,
    pub script: String,
    pub bucket: String,
    pub step: Step,
    pub secret_set: bool,
    pub setup_attempt: Option<String>,
    #[serde(default)]
    pub script_created_on: Option<String>,
    #[serde(default)]
    pub jurisdiction: Jurisdiction,
    #[serde(default)]
    pub bucket_created_on: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uninstall: Option<UninstallProgress>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
pub enum UninstallProgress {
    Cloud {
        server: Option<String>,
    },
    Service {
        server: Option<String>,
        #[serde(default)]
        bucket_kept: bool,
    },
    Complete {
        #[serde(default)]
        bucket_kept: bool,
    },
}

pub struct StateFile {
    path: PathBuf,
}

impl StateFile {
    pub fn new(paths: &Paths) -> Self {
        StateFile {
            path: paths.state_dir.join("setup-state.json"),
        }
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub fn load(&self) -> Result<Option<SetupState>, CliError> {
        match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| {
                CliError::from(format!(
                    "{} is not a valid setup state ({error}); delete it to start over",
                    self.path.display()
                )).human("Koidrive's saved setup progress cannot be read. Do not delete it or repeat cloud changes until you have checked what was created. Run koi diagnose to collect a report.")
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(CliError::from(format!("cannot read {}: {error}", self.path.display()))
                .human(format!("Could not read saved setup progress: {error}. Check local file permissions and available storage. Do not delete the saved progress. Run koi diagnose to collect a report."))),
        }
    }

    pub fn save(&self, state: &SetupState) -> Result<(), CliError> {
        let write = || -> io::Result<()> {
            let dir = self.path.parent().expect("the state file has a directory");
            DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
            let temporary = self.path.with_extension("json.tmp");
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(&serde_json::to_vec_pretty(state).expect("the state serializes"))?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)?;
            fs::File::open(dir)?.sync_all()
        };
        write().map_err(|error| CliError::from(format!("cannot write {}: {error}", self.path.display()))
            .human(format!("Could not save setup progress: {error}. Check local file permissions and available storage. Cloud changes may already have happened. Check the affected server in your account before repeating them. Run koi diagnose to collect a report.")))
    }

    pub fn remove(&self) -> Result<(), CliError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(CliError::from(format!("cannot delete {}: {error}", self.path.display()))
                .human(format!("Could not clear saved setup progress: {error}. Check local file permissions. Cloud changes may already be complete. Check the affected server in your account before repeating them. Run koi diagnose to collect a report."))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreadable_setup_progress_keeps_raw_details_without_advising_terminal_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let file = StateFile::new(&Paths::under(dir.path()));
        fs::create_dir_all(file.path().parent().unwrap()).unwrap();
        fs::write(file.path(), "{broken fixture}").unwrap();
        let error = file.load().unwrap_err();
        assert!(error.message.contains(&file.path().display().to_string()));
        assert!(error.message.contains("delete it to start over"));
        let human = error.human.unwrap();
        assert!(human.contains("Do not delete"));
        assert!(!human.contains(&file.path().display().to_string()));
        assert_eq!(error.code, koidrive_ipc::Code::Failed);
    }

    #[test]
    fn uninstall_phases_round_trip_and_older_progress_has_no_phase() {
        let dir = tempfile::tempdir().unwrap();
        let file = StateFile::new(&Paths::under(dir.path()));
        for phase in [
            UninstallProgress::Cloud {
                server: Some("https://koidrive.synthetic.workers.dev/".into()),
            },
            UninstallProgress::Service {
                server: Some("https://koidrive.synthetic.workers.dev/".into()),
                bucket_kept: true,
            },
            UninstallProgress::Complete { bucket_kept: true },
        ] {
            let state = SetupState {
                account: "account".into(),
                bucket: "remaining-bucket".into(),
                step: Step::Uninstalling,
                uninstall: Some(phase),
                ..SetupState::default()
            };
            file.save(&state).unwrap();
            assert_eq!(file.load().unwrap(), Some(state.clone()));
            let mut older = serde_json::to_value(&state).unwrap();
            older.as_object_mut().unwrap().remove("uninstall");
            fs::write(file.path(), older.to_string()).unwrap();
            assert_eq!(
                file.load().unwrap(),
                Some(SetupState {
                    uninstall: None,
                    ..state
                })
            );
        }
    }

    #[test]
    fn saves_loads_and_removes_the_state_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let file = StateFile::new(&paths);
        assert_eq!(file.load().unwrap(), None);
        let state = SetupState {
            account: "account".into(),
            script: "koidrive".into(),
            bucket: "koidrive-2".into(),
            step: Step::ScriptUploaded,
            secret_set: true,
            setup_attempt: Some("attempt".into()),
            script_created_on: Some("2026-11-10T01:02:03.456Z".into()),
            jurisdiction: Jurisdiction::Eu,
            bucket_created_on: Some("2026-11-10T01:02:00.000Z".into()),
            uninstall: None,
        };
        file.save(&state).unwrap();
        assert_eq!(file.load().unwrap(), Some(state));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(file.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
        file.remove().unwrap();
        file.remove().unwrap();
        assert_eq!(file.load().unwrap(), None);
    }
}
