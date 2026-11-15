// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::Path;
use std::time::Duration;

use super::command::{ok, remove_file, run, write};
use super::definition::plist_binary;
use super::report::SWITCHED_OFF;
use super::{Manager, Service, ServiceManager, State};

pub struct Launchd {
    pub(super) uid: u32,
}

impl Launchd {
    fn domain(&self) -> String {
        format!("gui/{}", self.uid)
    }

    fn target(&self, service: &Service) -> String {
        format!("{}/{}", self.domain(), service.label())
    }

    fn disabled(&self, service: &Service) -> Result<bool, String> {
        let disabled = ok("launchctl", &["print-disabled", &self.domain()])?;
        Ok(launchd_disabled(&disabled, &service.label()))
    }

    fn loaded(&self, service: &Service) -> Result<Option<String>, String> {
        let output = run("launchctl", &["print", &self.target(service)])?;
        Ok((output.status.success()).then(|| String::from_utf8_lossy(&output.stdout).into_owned()))
    }
}

impl Manager for Launchd {
    fn kind(&self) -> ServiceManager {
        ServiceManager::Launchd
    }

    fn name(&self, service: &Service) -> String {
        service.label()
    }

    fn state(&self, service: &Service) -> Result<State, String> {
        ok("launchctl", &["print", &self.domain()]).map_err(|_| {
            "this user is not logged in to the Mac's desktop, so launchd has no session to start it in"
                .to_owned()
        })?;
        Ok(match self.loaded(service)? {
            Some(job) => launchd_state(&job),
            None => unloaded(service.plist_file().exists(), self.disabled(service)?),
        })
    }

    fn installed(&self, service: &Service) -> Option<String> {
        plist_binary(&fs::read_to_string(service.plist_file()).ok()?)
    }

    fn crashed(&self, service: &Service) -> Option<String> {
        launchd_exit(&self.loaded(service).ok()??)
    }

    fn start(&self, service: &Service, restart: bool) -> Result<(), String> {
        if self.disabled(service)? {
            return Err(SWITCHED_OFF.trim_end().to_owned());
        }
        let log = service.log_file();
        let dir = log.parent().unwrap_or(Path::new("/"));
        fs::create_dir_all(dir)
            .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
        let plist = service.plist_file();
        let existed = plist.exists();
        let changed = write(&plist, &service.plist())?;
        let target = self.target(service);
        let loaded = self.loaded(service)?.is_some();
        if loaded && !changed {
            let args: &[&str] = match restart {
                true => &["kickstart", "-k", &target],
                false => &["kickstart", &target],
            };
            return ok("launchctl", args).map(drop);
        }
        if loaded {
            ok("launchctl", &["bootout", &target])?;
        }
        let path = plist.display().to_string();
        let mut attempts = 0;
        loop {
            match ok("launchctl", &["bootstrap", &self.domain(), &path]) {
                Ok(_) => return Ok(()),
                Err(_) if attempts < 3 => {
                    attempts += 1;
                    std::thread::sleep(Duration::from_millis(500));
                }
                Err(error) => {
                    if !existed {
                        remove_file(&plist)?;
                    }
                    return Err(format!(
                        "{error}. If koi is switched off in System Settings > General > Login Items > Allow in the Background, switch it on"
                    ));
                }
            }
        }
    }

    fn remove(&self, service: &Service) -> Result<(), String> {
        if self.loaded(service)?.is_some() {
            ok("launchctl", &["bootout", &self.target(service)])?;
        }
        remove_file(&service.plist_file())
    }
}

pub(super) fn unloaded(plist: bool, disabled: bool) -> State {
    match (plist, disabled) {
        (false, _) => State::NotInstalled,
        (true, true) => State::SwitchedOff,
        (true, false) => State::Stopped,
    }
}

fn launchd_field<'a>(job: &'a str, name: &str) -> Option<&'a str> {
    job.lines()
        .find_map(|line| line.trim().strip_prefix(name)?.strip_prefix(" = "))
}

pub(super) fn launchd_state(job: &str) -> State {
    match (launchd_field(job, "state"), launchd_exit(job)) {
        (Some("running"), _) => State::Running,
        (Some("SIGTERMed" | "spawn scheduled" | "xpcproxy"), None) => State::Starting,
        _ => State::Stopped,
    }
}

pub(super) fn launchd_exit(job: &str) -> Option<String> {
    if launchd_field(job, "state") == Some("running") {
        return None;
    }
    match launchd_field(job, "last exit code")?.parse::<i32>() {
        Ok(0) | Err(_) => None,
        Ok(code) => Some(format!("exit code {code}")),
    }
}

pub(super) fn launchd_disabled(disabled: &str, label: &str) -> bool {
    let entry = format!("\"{label}\" => ");
    disabled
        .lines()
        .any(|line| matches!(line.trim().strip_prefix(&entry), Some("disabled" | "true")))
}
