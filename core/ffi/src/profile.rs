// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use koidrive_driver::Paths;

use crate::types::SessionError;

const XDG: [&str; 3] = ["XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_RUNTIME_DIR"];

pub(crate) struct Profile {
    pub(crate) paths: Paths,
    pub(crate) env: Vec<(&'static str, Option<OsString>)>,
}

impl Profile {
    pub(crate) fn discover(
        helper: &Path,
        var: impl Fn(&str) -> Option<OsString>,
    ) -> Result<Self, SessionError> {
        let volatile = |path: &Path| {
            path.starts_with("/Volumes")
                || path
                    .components()
                    .any(|part| part.as_os_str() == "AppTranslocation")
        };
        if volatile(helper) || fs::canonicalize(helper).is_ok_and(|path| volatile(&path)) {
            return Err(SessionError::Failed {
                reason:
                    "Move Koidrive to Applications, then open it again to enable background sync."
                        .to_owned(),
            });
        }
        let env = (var("HOME").map(PathBuf::from))
            .filter(|home| home.is_absolute())
            .and_then(|home| agent_env(&home.join("Library/LaunchAgents")))
            .unwrap_or_default();
        let paths = Paths::from_vars(|name| match env.iter().find(|(set, _)| *set == name) {
            Some((_, value)) => value.clone(),
            None => var(name),
        })
        .map_err(|error| SessionError::Failed {
            reason: error.to_string(),
        })?;
        Ok(Profile { paths, env })
    }
}

fn agent_env(dir: &Path) -> Option<Vec<(&'static str, Option<OsString>)>> {
    let default = dir.join("dev.koidrive.koi.plist");
    let plist = match default.is_file() {
        true => default,
        false => {
            let mut agents = fs::read_dir(dir).ok()?.filter_map(|entry| {
                let name = entry.ok()?.file_name().into_string().ok()?;
                let ours = name.starts_with("dev.koidrive.koi.") && name.ends_with(".plist");
                ours.then(|| dir.join(name))
            });
            let only = agents.next()?;
            if agents.next().is_some() {
                return None;
            }
            only
        }
    };
    let text = koidrive_state_file::read_to_string(&plist).ok()?;
    let (_, after) = text.split_once("<key>EnvironmentVariables</key>")?;
    let (_, dict) = after.split_once("<dict>")?;
    let (dict, _) = dict.split_once("</dict>")?;
    Some(XDG.map(|name| (name, string_of(dict, name))).to_vec())
}

fn string_of(dict: &str, key: &str) -> Option<OsString> {
    let (_, after) = dict.split_once(&format!("<key>{key}</key>"))?;
    let (value, _) = after
        .trim_start()
        .strip_prefix("<string>")?
        .split_once("</string>")?;
    let value = (value.replace("&lt;", "<").replace("&gt;", ">")).replace("&amp;", "&");
    (!value.is_empty()).then(|| OsString::from(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plist(env: &[(&str, &str)]) -> String {
        let env: String = (env.iter())
            .map(|(name, value)| format!("\t\t<key>{name}</key>\n\t\t<string>{value}</string>\n"))
            .collect();
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">\n<dict>\n\t<key>Label</key>\n\t<string>dev.koidrive.koi</string>\n\t<key>EnvironmentVariables</key>\n\t<dict>\n\t\t<key>KOIDRIVE_LOG</key>\n\t\t<string>info</string>\n{env}\t</dict>\n\t<key>RunAtLoad</key>\n\t<true/>\n</dict>\n</plist>\n"
        )
    }

    fn discover(home: &Path, vars: &[(&str, &str)]) -> Profile {
        let home = home.to_str().unwrap().to_owned();
        let vars: Vec<(String, String)> = (vars.iter())
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        Profile::discover(
            Path::new("/Applications/Koidrive.app/Contents/Helpers/koi"),
            |name| match name {
                "HOME" => Some(home.clone().into()),
                name => (vars.iter())
                    .find(|(set, _)| set == name)
                    .map(|(_, value)| value.into()),
            },
        )
        .unwrap()
    }

    fn agent(home: &Path, label: &str, env: &[(&str, &str)]) {
        let dir = home.join("Library/LaunchAgents");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{label}.plist")), plist(env)).unwrap();
    }

    #[test]
    fn a_mounted_or_translocated_app_asks_to_move_without_changing_the_installed_profile() {
        let home = tempfile::tempdir().unwrap();
        let state = home.path().join("stable-state");
        agent(
            home.path(),
            "dev.koidrive.koi",
            &[("XDG_STATE_HOME", state.to_str().unwrap())],
        );
        let agent = home
            .path()
            .join("Library/LaunchAgents/dev.koidrive.koi.plist");
        let before = fs::read(&agent).unwrap();
        for helper in [
            "/Volumes/Koidrive/Koidrive.app/Contents/Helpers/koi",
            "/var/folders/ab/cd/T/AppTranslocation/123/d/Koidrive.app/Contents/Helpers/koi",
            "/private/var/folders/ab/cd/T/AppTranslocation/123/d/Koidrive.app/Contents/Helpers/koi",
        ] {
            let result = Profile::discover(Path::new(helper), |name| {
                (name == "HOME").then(|| home.path().as_os_str().to_owned())
            });
            assert!(matches!(
                result,
                Err(SessionError::Failed { reason })
                    if reason.contains("Move Koidrive to Applications")
            ));
            assert_eq!(fs::read(&agent).unwrap(), before);
        }
        for helper in [
            "/Applications/Koidrive.app/Contents/Helpers/koi",
            "/Users/ada/Applications/Koidrive.app/Contents/Helpers/koi",
            "/Users/ada/koidrive/target/debug/koi",
            "/Users/ada/Volumes/Koidrive.app/Contents/Helpers/koi",
            "/Volumes-backup/Koidrive.app/Contents/Helpers/koi",
            "/Applications/AppTranslocation.app/Contents/Helpers/koi",
        ] {
            let profile = Profile::discover(Path::new(helper), |name| {
                (name == "HOME").then(|| home.path().as_os_str().to_owned())
            })
            .unwrap();
            assert_eq!(profile.paths.state_dir, state.join("koidrive"));
        }
    }

    #[test]
    fn a_stable_looking_alias_to_a_translocated_app_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let translocated = dir.path().join("AppTranslocation/koi");
        fs::create_dir_all(translocated.parent().unwrap()).unwrap();
        fs::write(&translocated, "").unwrap();
        let alias = dir.path().join("koi");
        std::os::unix::fs::symlink(&translocated, &alias).unwrap();
        let result = Profile::discover(&alias, |_| None);
        assert!(matches!(result, Err(SessionError::Failed { .. })));
    }

    #[test]
    fn without_a_launch_agent_the_profile_is_the_one_the_environment_names() {
        let home = tempfile::tempdir().unwrap();
        let profile = discover(home.path(), &[]);
        assert_eq!(
            profile.paths.state_dir,
            home.path().join(".local/state/koidrive")
        );
        assert_eq!(
            profile.paths.socket(),
            home.path().join(".local/state/koidrive/daemon.sock")
        );
        assert!(profile.env.is_empty());

        let shell = discover(home.path(), &[("XDG_STATE_HOME", "/srv/ada/state")]);
        assert_eq!(shell.paths.state_dir, Path::new("/srv/ada/state/koidrive"));
    }

    #[test]
    fn the_launch_agent_names_the_profile_over_the_apps_own_environment() {
        let home = tempfile::tempdir().unwrap();
        agent(
            home.path(),
            "dev.koidrive.koi.1a2b3c4d",
            &[
                ("XDG_CONFIG_HOME", "/srv/ada/config"),
                ("XDG_STATE_HOME", "/srv/a&amp;b &lt;state&gt;"),
                ("XDG_RUNTIME_DIR", ""),
            ],
        );
        let profile = discover(home.path(), &[("XDG_RUNTIME_DIR", "/run/user/501")]);
        assert_eq!(
            profile.paths.state_dir,
            Path::new("/srv/a&b <state>/koidrive")
        );
        assert_eq!(
            profile.paths.config_file,
            Path::new("/srv/ada/config/koidrive/config.toml")
        );
        assert_eq!(
            profile.paths.socket(),
            Path::new("/srv/a&b <state>/koidrive/daemon.sock"),
            "an empty XDG_RUNTIME_DIR in the agent is unset for the app too"
        );
        assert_eq!(
            profile.env,
            [
                ("XDG_CONFIG_HOME", Some("/srv/ada/config".into())),
                ("XDG_STATE_HOME", Some("/srv/a&b <state>".into())),
                ("XDG_RUNTIME_DIR", None),
            ]
        );
    }

    #[test]
    fn the_default_agent_wins_and_two_others_name_no_profile() {
        let home = tempfile::tempdir().unwrap();
        agent(
            home.path(),
            "dev.koidrive.koi.1a2b",
            &[("XDG_STATE_HOME", "/srv/one")],
        );
        agent(
            home.path(),
            "dev.koidrive.koi.3c4d",
            &[("XDG_STATE_HOME", "/srv/two")],
        );
        agent(
            home.path(),
            "dev.koidrive.sccache",
            &[("XDG_STATE_HOME", "/srv/other")],
        );
        let ambiguous = discover(home.path(), &[]);
        assert_eq!(
            ambiguous.paths.state_dir,
            home.path().join(".local/state/koidrive")
        );
        assert!(ambiguous.env.is_empty());

        let state = home.path().join("state");
        agent(
            home.path(),
            "dev.koidrive.koi",
            &[("XDG_STATE_HOME", state.to_str().unwrap())],
        );
        assert_eq!(
            discover(home.path(), &[]).paths.state_dir,
            state.join("koidrive")
        );
    }
}
