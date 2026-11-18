// SPDX-License-Identifier: Apache-2.0

use crate::error::CliError;
use crate::formatting::printable;
use crate::report::{AccountData, Done};
use koidrive_driver::{Credential, DriverError, Paths};
use koidrive_ipc::Response;
use koidrive_net::{Auth, Reload};
use std::io;
use std::sync::Arc;

pub(crate) fn unexpected(response: Response) -> CliError {
    CliError::from(format!("the daemon answered {response:?}"))
        .human("Background sync returned an unexpected response. The command's result is unknown. Run koi status to check sync, and check the affected item before repeating a command that changes it. Run koi diagnose to collect a report.")
}

pub(crate) fn paired(paths: &Paths, response: Response) -> Result<Done, CliError> {
    let Response::Paired { account } = response else {
        return Err(unexpected(response));
    };
    let text = format!(
        "Paired as {} ({}) with {}.\nThe credential is in {}.\n",
        printable(&account.device_name),
        printable(&account.device_id),
        printable(&account.server),
        printable(&paths.credential().display().to_string())
    );
    Ok(Done::ok(AccountData::of(&account), text).human(
        format!(
            "Connected this device as {}.\n",
            printable(&account.device_name)
        ),
        "",
    ))
}

/// This profile's credential, for the commands that talk to the server directly. They write nothing to the profile, so
/// they run the same whether or not the daemon holds it.
pub(crate) fn signed_in(paths: &Paths) -> Result<Auth, CliError> {
    let credential = Credential::load(&paths.credential())
        .map_err(DriverError::from)?
        .ok_or(DriverError::NotPaired)?;
    reading(paths, &credential)
}

/// An [`Auth`] for a command that only reads the profile's credential. The process that holds the profile may rotate
/// the credential meanwhile, so when the server refuses it, it reads both credential files again, once, and presents
/// what they hold (RFC 0011 section 2). A reload that finds a credential for another server fails, so that no
/// credential reaches a server it was not issued by.
pub(crate) fn reading(paths: &Paths, credential: &Credential) -> Result<Auth, CliError> {
    let path = paths.credential();
    let server = credential.server.clone();
    let reload: Reload = Arc::new(move || {
        let stored = Credential::load_stored(&path)
            .map_err(io::Error::other)?
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, DriverError::NotPaired.to_string())
            })?;
        if stored.credential.server != server {
            return Err(io::Error::other(
                "the profile now holds a credential for another server",
            ));
        }
        Ok(stored.net())
    });
    Ok(Auth::reloading(
        credential.api().map_err(CliError::from)?,
        credential.net(),
        reload,
    ))
}
