// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use koidrive_driver::Paths;

use super::systemd::systemctl;
use super::{ServiceManager, UNIT};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Service {
    pub(super) suffix: Option<String>,
    pub(super) binary: String,
    pub(super) home: PathBuf,
    pub(super) paths: Paths,
    pub(super) env: Vec<(&'static str, Option<String>)>,
}

impl Service {
    pub fn new(paths: &Paths, home: &Path, binary: &Path) -> Result<Self, String> {
        let text = |path: &Path| match path.to_str() {
            Some(text) if !text.chars().any(char::is_control) => Ok(text.to_owned()),
            _ => Err(format!(
                "{} cannot be written into a service file",
                path.display()
            )),
        };
        let parent = |path: &Path| path.parent().map(Path::to_path_buf).unwrap_or_default();
        let config_home = parent(&parent(&paths.config_file));
        let state_home = parent(&paths.state_dir);
        let runtime = match paths.runtime_dir == paths.state_dir {
            true => None,
            false => Some(text(&parent(&paths.runtime_dir))?),
        };
        let default = paths.state_dir == home.join(".local/state/koidrive")
            && paths.config_file == home.join(".config/koidrive/config.toml");
        let suffix = (!default).then(|| paths.state_key());
        let binary = crate::update::homebrew_link(binary).unwrap_or_else(|| binary.to_path_buf());
        Ok(Service {
            suffix,
            binary: text(&binary)?,
            home: home.to_path_buf(),
            paths: paths.clone(),
            env: vec![
                ("XDG_CONFIG_HOME", Some(text(&config_home)?)),
                ("XDG_STATE_HOME", Some(text(&state_home)?)),
                ("XDG_RUNTIME_DIR", runtime),
            ],
        })
    }

    pub fn unit_name(&self) -> String {
        match &self.suffix {
            None => "koidrive.service".to_owned(),
            Some(suffix) => format!("koidrive-{suffix}.service"),
        }
    }

    pub fn label(&self) -> String {
        match &self.suffix {
            None => "dev.koidrive.koi".to_owned(),
            Some(suffix) => format!("dev.koidrive.koi.{suffix}"),
        }
    }

    pub(super) fn unit_file(&self) -> PathBuf {
        self.paths.state_dir.join("systemd").join(self.unit_name())
    }

    pub(crate) fn write_unit(&self) -> Result<bool, String> {
        let file = self.unit_file();
        let dir = file.parent().expect("the unit file has a folder");
        (self.paths.create_state_dir(dir))
            .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
        super::command::write(&file, &self.unit())
    }

    pub(super) fn plist_file(&self) -> PathBuf {
        (self.home.join("Library/LaunchAgents")).join(format!("{}.plist", self.label()))
    }

    pub fn file(&self, manager: ServiceManager) -> PathBuf {
        match manager {
            ServiceManager::Launchd => self.plist_file(),
            ServiceManager::Systemd => self.unit_file(),
        }
    }

    pub fn log_file(&self) -> PathBuf {
        (self.home.join("Library/Logs/Koidrive")).join(format!("{}.log", self.label()))
    }

    pub fn unit(&self) -> String {
        let mut unit = String::new();
        for line in UNIT.lines() {
            match line.strip_prefix("ExecStart=") {
                Some(_) => unit.push_str(&format!(
                    "ExecStart={} _daemon\n",
                    systemd_quoted(&self.binary)
                )),
                None => unit.push_str(&format!("{line}\n")),
            }
            if line == "[Service]" {
                for (name, value) in &self.env {
                    unit.push_str(&match value {
                        Some(value) => format!(
                            "Environment={}\n",
                            systemd_quoted(&format!("{name}={value}"))
                        ),
                        None => format!("UnsetEnvironment={name}\n"),
                    });
                }
            }
        }
        unit
    }

    /// The launch agent's property list. A plist cannot unset a variable that launchd's own environment sets, so an
    /// unset one is written empty, which the daemon reads as unset.
    pub fn plist(&self) -> String {
        let mut env = String::from("\t\t<key>KOIDRIVE_LOG</key>\n\t\t<string>info</string>\n");
        for (name, value) in &self.env {
            env.push_str(&format!(
                "\t\t<key>{name}</key>\n\t\t<string>{}</string>\n",
                xml(value.as_deref().unwrap_or_default())
            ));
        }
        let log = xml(&self.log_file().display().to_string());
        let keep_alive = match (self.env.iter()).any(|(name, _)| *name == crate::lifeline::LIFELINE)
        {
            true => "<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n\t</dict>",
            false => "<true/>",
        };
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{binary}</string>
		<string>_daemon</string>
	</array>
	<key>EnvironmentVariables</key>
	<dict>
{env}	</dict>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	{keep_alive}
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
</dict>
</plist>
"#,
            label = self.label(),
            binary = xml(&self.binary),
        )
    }

    pub(super) fn log(&self, manager: ServiceManager) -> String {
        match manager {
            ServiceManager::Launchd => self.log_file().display().to_string(),
            _ => format!("journalctl --user -u {}", self.unit_name()),
        }
    }
}

/// A quoted word of a unit file: `%` starts a specifier, and quotes and backslashes need escapes.
fn systemd_quoted(text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!("\"{escaped}\"")
}

pub(super) fn unit_binary(unit: &str) -> Option<String> {
    let exec = unit
        .lines()
        .find_map(|line| line.strip_prefix("ExecStart="))?;
    let quoted = exec
        .strip_suffix(" _daemon")?
        .strip_prefix('"')?
        .strip_suffix('"')?;
    let mut binary = String::new();
    let mut chars = quoted.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' | '%' => binary.push(chars.next()?),
            c => binary.push(c),
        }
    }
    Some(binary)
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub(super) fn plist_binary(plist: &str) -> Option<String> {
    let (_, arguments) = plist.split_once("<key>ProgramArguments</key>")?;
    let (_, binary) = arguments.split_once("<string>")?;
    let (binary, _) = binary.split_once("</string>")?;
    Some(
        binary
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&"),
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installed {
    pub name: String,
    pub binary: Option<String>,
    pub env: Vec<(&'static str, Option<String>)>,
}

const PROFILE_VARS: [&str; 3] = ["XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_RUNTIME_DIR"];

pub(super) fn suffixed(name: &str, stem: &str, end: &str) -> bool {
    let Some(rest) = name
        .strip_prefix(stem)
        .and_then(|rest| rest.strip_suffix(end))
    else {
        return false;
    };
    rest.is_empty()
        || rest.strip_prefix(['.', '-']).is_some_and(|key| {
            key.len() == 8 && key.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
        })
}

pub fn installed_services(manager: ServiceManager, home: &Path) -> Result<Vec<Installed>, String> {
    let read = |path: &Path| {
        fs::read_to_string(path).map_err(|error| format!("cannot read {}: {error}", path.display()))
    };
    let mut found = Vec::new();
    match manager {
        ServiceManager::Launchd => {
            let dir = home.join("Library/LaunchAgents");
            for entry in entries_of(&dir)? {
                let Some(name) = (entry.file_name().to_str())
                    .filter(|file| suffixed(file, "dev.koidrive.koi", ".plist"))
                    .and_then(|file| file.strip_suffix(".plist"))
                    .map(str::to_owned)
                else {
                    continue;
                };
                let text = read(&entry.path())?;
                found.push(Installed {
                    name,
                    binary: plist_binary(&text),
                    env: plist_env(&text),
                });
            }
        }
        ServiceManager::Systemd => {
            match systemctl(&[
                "list-unit-files".into(),
                "--no-legend".into(),
                "koidrive*.service".into(),
            ]) {
                Ok(units) => {
                    for name in units
                        .lines()
                        .filter_map(|line| line.split_whitespace().next())
                    {
                        if !suffixed(name, "koidrive", ".service") {
                            continue;
                        }
                        let text = systemctl(&["cat".into(), name.into()])?;
                        found.push(Installed {
                            name: name.to_owned(),
                            binary: unit_binary(&text),
                            env: unit_env(&text),
                        });
                    }
                }
                Err(_) => {
                    for entry in entries_of(&home.join(".config/systemd/user"))? {
                        let Some(name) = (entry.file_name().to_str())
                            .filter(|file| suffixed(file, "koidrive", ".service"))
                            .map(str::to_owned)
                        else {
                            continue;
                        };
                        let text = match fs::read_to_string(entry.path()) {
                            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                            text => text.map_err(|error| {
                                format!("cannot read {}: {error}", entry.path().display())
                            })?,
                        };
                        found.push(Installed {
                            name,
                            binary: unit_binary(&text),
                            env: unit_env(&text),
                        });
                    }
                }
            }
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(found)
}

fn entries_of(dir: &Path) -> Result<Vec<fs::DirEntry>, String> {
    let shown = |error: io::Error| format!("cannot list {}: {error}", dir.display());
    match fs::read_dir(dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        entries => entries
            .map_err(shown)?
            .collect::<io::Result<_>>()
            .map_err(shown),
    }
}

pub(super) fn plist_env(plist: &str) -> Vec<(&'static str, Option<String>)> {
    let Some(dict) = plist
        .split_once("<key>EnvironmentVariables</key>")
        .and_then(|(_, after)| after.split_once("<dict>"))
        .and_then(|(_, dict)| dict.split_once("</dict>"))
        .map(|(dict, _)| dict)
    else {
        return Vec::new();
    };
    PROFILE_VARS
        .into_iter()
        .filter_map(|name| {
            let (_, after) = dict.split_once(&format!("<key>{name}</key>"))?;
            let (value, _) = after
                .trim_start()
                .strip_prefix("<string>")?
                .split_once("</string>")?;
            let value = (value.replace("&lt;", "<").replace("&gt;", ">")).replace("&amp;", "&");
            Some((name, (!value.is_empty()).then_some(value)))
        })
        .collect()
}

pub(super) fn unit_env(unit: &str) -> Vec<(&'static str, Option<String>)> {
    let unquoted = |word: &str| -> Option<String> {
        let mut text = String::new();
        let mut chars = word.strip_prefix('"')?.strip_suffix('"')?.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' | '%' => text.push(chars.next()?),
                c => text.push(c),
            }
        }
        Some(text)
    };
    PROFILE_VARS
        .into_iter()
        .filter_map(|name| {
            unit.lines().find_map(|line| {
                if line.strip_prefix("UnsetEnvironment=") == Some(name) {
                    return Some((name, None));
                }
                let set = unquoted(line.strip_prefix("Environment=")?)?;
                let value = set.strip_prefix(name)?.strip_prefix('=')?;
                Some((name, Some(value.to_owned())))
            })
        })
        .collect()
}
