// SPDX-License-Identifier: Apache-2.0

use std::fs;

use super::command::{ok, remove_file, run};
use super::definition::unit_binary;
use super::{Manager, Service, ServiceManager, State};

pub struct Systemd {
    pub(super) uid: u32,
}

pub(super) fn systemctl(args: &[&str]) -> Result<String, String> {
    let args: Vec<&str> = ["--user"].iter().chain(args).copied().collect();
    ok("systemctl", &args)
}

impl Manager for Systemd {
    fn kind(&self) -> ServiceManager {
        ServiceManager::Systemd
    }

    fn name(&self, service: &Service) -> String {
        service.unit_name()
    }

    fn state(&self, service: &Service) -> Result<State, String> {
        systemctl(&["show-environment"]).map_err(|error| {
            format!("there is no systemd user session to start it in ({error})")
        })?;
        let active = run(
            "systemctl",
            &["--user", "is-active", "--quiet", &service.unit_name()],
        )?;
        Ok(match active.status.success() {
            true => State::Running,
            false if service.unit_file().exists() => State::Stopped,
            false => State::NotInstalled,
        })
    }

    fn installed(&self, service: &Service) -> Option<String> {
        unit_binary(&fs::read_to_string(service.unit_file()).ok()?)
    }

    fn start(&self, service: &Service, restart: bool) -> Result<(), String> {
        let file = service.unit_file();
        let changed = service.write_unit()?;
        systemctl(&["enable", &file.display().to_string()])?;
        systemctl(&["daemon-reload"])?;
        let verb = if changed || restart {
            "restart"
        } else {
            "start"
        };
        systemctl(&[verb, &service.unit_name()]).map(drop)
    }

    fn remove(&self, service: &Service) -> Result<(), String> {
        let name = service.unit_name();
        if run("systemctl", &["--user", "cat", &name])?
            .status
            .success()
        {
            systemctl(&["disable", "--now", &name])?;
        }
        remove_file(&service.unit_file())?;
        systemctl(&["daemon-reload"]).map(drop)
    }

    fn lingers(&self) -> Option<bool> {
        let uid = self.uid.to_string();
        let linger = ok(
            "loginctl",
            &["show-user", &uid, "--property=Linger", "--value"],
        )
        .ok()?;
        match linger.as_str() {
            "yes" => Some(true),
            "no" => Some(false),
            _ => None,
        }
    }

    /// Turns lingering on where polkit allows it without a password, as it does by default in a local session.
    fn linger(&self) -> Result<(), String> {
        ok("loginctl", &["--no-ask-password", "enable-linger"]).map(drop)
    }
}
