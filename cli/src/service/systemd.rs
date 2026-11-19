// SPDX-License-Identifier: Apache-2.0

use super::command::{ok, remove_file, run};
use super::definition::unit_binary;
use super::{Manager, Service, ServiceManager, State};
use crate::program::{Arg, Confined};

pub struct Systemd {
    pub(super) uid: u32,
}

pub(super) fn systemctl(args: &[Arg]) -> Result<String, String> {
    let args: Vec<Arg> = [Arg::from("--user")]
        .into_iter()
        .chain(args.iter().cloned())
        .collect();
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
        systemctl(&["show-environment".into()]).map_err(|error| {
            format!("there is no systemd user session to start it in ({error})")
        })?;
        let active = run(
            "systemctl",
            &[
                "--user".into(),
                "is-active".into(),
                "--quiet".into(),
                service.unit_name().into(),
            ],
        )?;
        Ok(match active.status.success() {
            true => State::Running,
            false if service.unit_file().exists() => State::Stopped,
            false => State::NotInstalled,
        })
    }

    fn installed(&self, service: &Service) -> Option<String> {
        unit_binary(&koidrive_state_file::read_following(&service.unit_file()).ok()?)
    }

    fn start(&self, service: &Service, restart: bool) -> Result<(), String> {
        let file = service.unit_file();
        let changed = service.write_unit()?;
        let file = Confined::new(&file).map_err(|error| error.to_string())?;
        systemctl(&["enable".into(), file.into()])?;
        systemctl(&["daemon-reload".into()])?;
        let verb = if changed || restart {
            "restart"
        } else {
            "start"
        };
        systemctl(&[verb.into(), service.unit_name().into()]).map(drop)
    }

    fn remove(&self, service: &Service) -> Result<(), String> {
        let name = service.unit_name();
        if run(
            "systemctl",
            &["--user".into(), "cat".into(), (&name).into()],
        )?
        .status
        .success()
        {
            systemctl(&["disable".into(), "--now".into(), name.into()])?;
        }
        remove_file(&service.unit_file())?;
        systemctl(&["daemon-reload".into()]).map(drop)
    }

    fn lingers(&self) -> Option<bool> {
        let uid = self.uid.to_string();
        let linger = ok(
            "loginctl",
            &[
                "show-user".into(),
                uid.into(),
                "--property=Linger".into(),
                "--value".into(),
            ],
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
        ok(
            "loginctl",
            &["--no-ask-password".into(), "enable-linger".into()],
        )
        .map(drop)
    }
}
