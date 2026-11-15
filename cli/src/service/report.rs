// SPDX-License-Identifier: Apache-2.0

use koidrive_ipc::Blocked;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Manager, Service, ServiceManager, State};

/// The service as a command left it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Report {
    pub manager: ServiceManager,
    pub name: String,
    pub state: State,
    /// Why the service cannot run, or did not change as it should.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    #[serde(skip)]
    pub(super) summary_problem: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lingers: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked: Vec<Blocked>,
    #[serde(skip)]
    pub(super) started: bool,
    #[serde(skip)]
    pub(super) removed: bool,
    /// The folders the profile links, as the command that brought the service in line counted them. Left out when it
    /// links none or the command did not count them.
    #[serde(default, skip_serializing_if = "no_folders")]
    #[schemars(with = "usize")]
    pub(super) folders: Option<usize>,
    #[serde(skip)]
    pub(super) log: String,
}

fn no_folders(folders: &Option<usize>) -> bool {
    folders.is_none_or(|count| count == 0)
}

impl Report {
    pub fn summary(&self) -> String {
        let mut text = match self.state {
            State::Running if self.problem.is_some() => "Background sync needs attention.\n".to_owned(),
            State::Running => "Koidrive is running in the background.\n".to_owned(),
            State::Starting => "Background sync is starting.\n".to_owned(),
            State::Stopped => "Background sync is stopped. Run koi service start.\n".to_owned(),
            State::SwitchedOff => SWITCHED_OFF.to_owned(),
            State::NotInstalled if self.removed => "Stopped and removed the background service.\n".to_owned(),
            State::NotInstalled => "Background sync starts when you link a folder.\n".to_owned(),
            State::Unavailable => "Background sync is unavailable.\n".to_owned(),
            State::Off => "Background sync is off in config.toml. Set service.background to true, then run koi service start.\n".to_owned(),
        };
        if let Some(problem) = self.summary_problem.as_ref().or(self.problem.as_ref()) {
            text.push_str(&format!("Reason: {}\nUse --verbose for service details. After fixing the problem, run koi service start.\n", crate::printable(problem)));
        }
        if self.lingers == Some(false) {
            text.push_str("It stops when you log out. To keep it running, run: sudo loginctl enable-linger \"$USER\"\n");
        }
        text.push_str(&blocked_text(&self.blocked));
        text
    }

    pub(super) fn new(manager: &dyn Manager, name: String, state: State) -> Self {
        Report {
            manager: manager.kind(),
            name,
            state,
            problem: None,
            summary_problem: None,
            lingers: None,
            blocked: Vec::new(),
            started: false,
            removed: false,
            folders: None,
            log: String::new(),
        }
    }

    pub(super) fn unavailable(manager: &dyn Manager, problem: String) -> Self {
        Report {
            problem: Some(problem),
            ..Report::new(manager, String::new(), State::Unavailable)
        }
    }

    pub(super) fn troubled(
        manager: &dyn Manager,
        service: Option<&Service>,
        problem: String,
    ) -> Self {
        let Some(service) = service else {
            return Report::unavailable(manager, problem);
        };
        let state = manager.state(service).unwrap_or(State::Unavailable);
        Report {
            problem: Some(problem),
            log: service.log(manager.kind()),
            ..Report::new(manager, manager.name(service), state)
        }
    }

    pub fn retryable(&self) -> bool {
        match self.state {
            State::Starting => true,
            State::Running => self.problem.is_some(),
            _ => false,
        }
    }

    pub fn text(&self) -> String {
        let mut text = String::new();
        let launchd = self.manager == ServiceManager::Launchd;
        let folders = self.folders.unwrap_or(0);
        let problem = (self.problem.as_deref()).map(crate::printable);
        match self.state {
            State::Off if folders > 0 => text.push_str(
                "Background sync is off in config.toml, so folders sync only while koi _daemon runs.\n",
            ),
            State::Unavailable if folders > 0 || self.folders.is_none() && self.problem.is_some() => {
                text.push_str(&format!(
                    "Koidrive can't sync in the background: {}.\n",
                    problem.as_deref().unwrap_or("no service manager")
                ));
                text.push_str(match launchd {
                    true => "Log in to this Mac's desktop, then run: koi service start\n",
                    false => "Log in as this user directly rather than through su or sudo, or run sudo loginctl enable-linger \"$USER\" and log in again. Then run: koi service start\nWithout systemd, keep koi _daemon running with your own service manager.\n",
                });
            }
            State::Running if self.started => {
                text.push_str(&format!("Syncing in the background ({}).\n", self.name));
                text.push_str(match (launchd, self.lingers) {
                    (true, _) => "It starts again when you log in. If macOS shows \"Background Items Added\", leave koi switched on in System Settings > General > Login Items.\n",
                    (false, Some(false)) => "It stops when you log out and starts at your next login. To keep it running without a login, run: sudo loginctl enable-linger \"$USER\"\n",
                    (false, Some(true)) => "It keeps running after you log out and starts again after a restart.\n",
                    (false, None) => "",
                });
            }
            State::Stopped if folders > 0 => text.push_str(&format!(
                "The background service {} is installed but not running. Its log: {}\nTo start it again, run: koi service start\n",
                self.name, self.log
            )),
            State::SwitchedOff if folders > 0 => text.push_str(SWITCHED_OFF),
            State::NotInstalled if self.removed => {
                text.push_str("Stopped and removed the background service.\n")
            }
            _ => {}
        }
        if let Some(problem) = problem.filter(|_| self.state != State::Unavailable) {
            text.push_str(&format!("The background service: {problem}\n"));
        }
        text.push_str(&blocked_text(&self.blocked));
        text
    }

    pub fn failed(&self) -> bool {
        self.problem.is_some()
            || matches!(
                self.state,
                State::Stopped | State::SwitchedOff | State::Unavailable
            )
    }

    pub fn failure_message(&self) -> String {
        if self.state == State::SwitchedOff && self.problem.is_none() {
            return SWITCHED_OFF.trim_end().to_owned();
        }
        self.problem
            .clone()
            .unwrap_or_else(|| self.status_line())
            .lines()
            .map(str::trim)
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn status_line(&self) -> String {
        let state = match self.state {
            State::Running => format!("running ({})", self.name),
            State::Starting => format!("starting ({})", self.name),
            State::Stopped => format!("installed, not running ({}); log: {}", self.name, self.log),
            State::SwitchedOff => format!("switched off in Login Items ({})", self.name),
            State::NotInstalled => "not installed; it starts when you link a folder".to_owned(),
            State::Unavailable => format!(
                "unavailable: {}",
                crate::printable(self.problem.as_deref().unwrap_or_default())
            ),
            State::Off => "off in config.toml".to_owned(),
        };
        let mut line = format!("Background service: {state}\n");
        if self.state == State::SwitchedOff {
            line.push_str(&format!("  {SWITCHED_OFF}"));
        }
        if self.lingers == Some(false) {
            line.push_str("  It stops when you log out. To keep it running, run: sudo loginctl enable-linger \"$USER\"\n");
        }
        line.push_str(&blocked_text(&self.blocked));
        line
    }
}

pub(super) const SWITCHED_OFF: &str = "koi is switched off in System Settings > General > Login Items > Allow in the Background, so the folders don't sync. To sync them in the background, switch it on, then run: koi service start\n";

pub(super) fn blocked_text(blocked: &[Blocked]) -> String {
    let mut text = String::new();
    for blocked in blocked {
        let folder = crate::printable(&blocked.folder);
        let word = crate::pairing::shell_word(&folder);
        text.push_str(&match blocked.waiting {
            true => format!("koi is still waiting to read {folder}. If macOS asks whether koi may access it, click Allow, so that it syncs in the background.\n"),
            false => format!("macOS doesn't let koi read {folder} in the background. Open System Settings > Privacy & Security > Files & Folders and switch the folder on under koi, or add koi under Full Disk Access. Then run: koi folder restart {word}\n"),
        });
    }
    text
}
