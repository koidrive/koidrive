// SPDX-License-Identifier: Apache-2.0

use std::cell::{Cell, RefCell};

use std::collections::VecDeque;
use std::fs;

use std::path::Path;

use std::process::{Command, Stdio};

use super::binary::{MOVE_TO_APPLICATIONS, PROBE, target, volatile_binary};

use super::definition::{plist_binary, plist_env, suffixed, unit_binary, unit_env};

use super::launchd::{launchd_disabled, launchd_exit, launchd_state, unloaded};

use super::report::{SWITCHED_OFF, blocked_text};

use super::*;

fn paths(home: &str, vars: &[(&str, &str)]) -> Paths {
    Paths::from_vars(|name| {
        (vars.iter().chain(&[("HOME", home)]))
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.into())
    })
    .unwrap()
}

fn service(vars: &[(&str, &str)]) -> Service {
    let home = "/home/ada";
    let binary = Path::new("/home/ada/.local/bin/koi");
    Service::new(&paths(home, vars), Path::new(home), binary).unwrap()
}

#[derive(Default)]
struct Fake {
    unavailable: bool,
    unit: RefCell<Option<String>>,
    running: Cell<bool>,
    restarts: Cell<u32>,
    lingers: Cell<Option<bool>>,
    linger_refused: bool,
    spawning: Cell<u32>,
    switched_off: Cell<bool>,
    remove_problem: Option<&'static str>,
    service_lock: Option<PathBuf>,
    locked_at_start: Cell<Option<bool>>,
    crashed: Option<&'static str>,
    script: RefCell<VecDeque<State>>,
}

impl Manager for Fake {
    fn kind(&self) -> ServiceManager {
        ServiceManager::Systemd
    }

    fn name(&self, service: &Service) -> String {
        service.unit_name()
    }

    fn state(&self, _: &Service) -> Result<State, String> {
        if let Some(state) = self.script.borrow_mut().pop_front() {
            return Ok(state);
        }
        if self.unavailable {
            return Err("no user bus".to_owned());
        }
        let spawning = self.spawning.get();
        self.spawning.set(spawning.saturating_sub(1));
        Ok(match (self.unit.borrow().is_some(), self.running.get()) {
            (true, _) if self.switched_off.get() => State::SwitchedOff,
            (true, true) if spawning > 0 => State::Stopped,
            (_, true) => State::Running,
            (true, false) => State::Stopped,
            (false, false) => State::NotInstalled,
        })
    }

    fn crashed(&self, _: &Service) -> Option<String> {
        self.crashed.map(str::to_owned)
    }

    fn installed(&self, _: &Service) -> Option<String> {
        unit_binary(self.unit.borrow().as_deref()?)
    }

    fn start(&self, service: &Service, restart: bool) -> Result<(), String> {
        if let Some(path) = &self.service_lock {
            let file = (fs::File::options().create(true).truncate(false).write(true))
                .open(path)
                .unwrap();
            let taken =
                rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive);
            self.locked_at_start.set(Some(taken.is_err()));
        }
        let unit = Some(service.unit());
        if (*self.unit.borrow() != unit || restart) && self.running.get() {
            self.restarts.set(self.restarts.get() + 1);
        }
        *self.unit.borrow_mut() = unit;
        self.running.set(true);
        self.switched_off.set(false);
        Ok(())
    }

    fn remove(&self, _: &Service) -> Result<(), String> {
        if let Some(problem) = self.remove_problem {
            return Err(problem.into());
        }
        *self.unit.borrow_mut() = None;
        self.running.set(false);
        Ok(())
    }

    fn lingers(&self) -> Option<bool> {
        self.lingers.get()
    }

    fn linger(&self) -> Result<(), String> {
        if self.linger_refused {
            return Err("interactive authentication required".to_owned());
        }
        self.lingers.set(Some(true));
        Ok(())
    }
}

fn version(text: &str) -> Version {
    Version::parse(text).unwrap()
}

fn koi_printing(dir: &Path, name: &str, printed: &str, status: u8) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    fs::write(
        &path,
        format!("#!/bin/sh\nprintf '{printed}'\nexit {status}\n"),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn running_on(fake: &Fake, binary: &Path) {
    let home = "/home/ada";
    let installed = Service::new(&paths(home, &[]), Path::new(home), binary).unwrap();
    *fake.unit.borrow_mut() = Some(installed.unit());
    fake.running.set(true);
}

fn runs(fake: &Fake) -> String {
    unit_binary(fake.unit.borrow().as_deref().unwrap()).unwrap()
}

fn unreadable_stores(paths: &Paths) {
    let store = paths.store(crate::ipc::tests::SPACE).unwrap();
    for suffix in ["-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", store.display()));
    }
    fs::write(&store, b"a store only a newer koi reads").unwrap();
}
