// SPDX-License-Identifier: Apache-2.0

use crate::error::CliError;
use crate::formatting::{ago, now_ms, printable};
use crate::options::RequestArg;
use crate::profile::{signed_in, unexpected};
use crate::report::{
    DeviceData, DevicesData, Done, InvitationData, LeftData, NotRemovedData, RevokedData,
    SetAsideData, UnconfirmedData,
};
use crate::request::Begun;
use crate::{ipc, output, pairing, request};
use clap::Subcommand;
use koidrive_driver::Paths;
use koidrive_ipc::{Code, Key, Request, RequestId, Response};
use koidrive_net::{ApiError, Auth};
use koidrive_proto::v1::DeviceInfo;
use serde::{Deserialize, Serialize};

#[derive(Subcommand)]
pub(crate) enum DeviceAction {
    /// Show a pairing code that adds another device to this server. It works once, for 10 minutes.
    Invite {
        #[command(flatten)]
        request: RequestArg,
    },
    /// List the devices paired with the server, revoked ones included.
    List,
    /// Revoke another device, such as a lost one. It stops syncing at once, and the files on it stay there.
    Revoke {
        /// The device's ID, as koi device list shows it.
        #[arg(value_parser = device_id)]
        id: String,
    },
    /// Sign this device out: revoke it on the server and unlink every folder. The files stay where they are.
    Leave {
        /// Sign out even if this is the account's last active device or the server does not answer.
        #[arg(long)]
        last_device: bool,
    },
}

/// A device ID as the server issues it: a lowercase UUID. Checked here so that the argument cannot name another path.
pub(crate) fn device_id(text: &str) -> Result<String, String> {
    let id = text.to_ascii_lowercase();
    let shaped = id.len() == 36
        && id.char_indices().all(|(at, c)| match at {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        });
    if shaped {
        Ok(id)
    } else {
        Err("not a device ID; koi device list shows them".to_owned())
    }
}

pub(crate) fn still_listed(unconfirmed: &koidrive_driver::Unconfirmed) -> String {
    format!(
        "The server did not confirm that it revoked this device, so it may still list it. To remove it, run this on another device signed in to {}: koi device revoke {}\nIf you have no other device, add one with koi server add-device --interactive and run the command there.\n",
        printable(&unconfirmed.server),
        crate::output::command_word(&unconfirmed.device_id)
    )
}

pub(crate) fn left(
    not_removed: Vec<koidrive_driver::CleanupFailure>,
    unconfirmed: Option<koidrive_driver::Unconfirmed>,
    set_aside: &koidrive_driver::SetAside,
) -> Done {
    let set_aside = SetAsideData::of(set_aside);
    let mut text = "Signed out. This device no longer syncs, and the files in its folders stay where they are.\n".to_owned();
    text.push_str(&set_aside.lines());
    if let Some(unconfirmed) = &unconfirmed {
        text.push_str(&still_listed(unconfirmed));
    }
    let mut summary = text.clone();
    if let Some(unconfirmed) = &unconfirmed {
        text.push_str(&format!("Reason: {}.\n", printable(&unconfirmed.reason)));
    }
    let tone = if not_removed.is_empty() && unconfirmed.is_none() && set_aside.kept.is_empty() {
        output::Tone::Completed
    } else if not_removed.is_empty() {
        output::Tone::Attention
    } else {
        summary.push_str("Sign-out cleanup is incomplete. Run koi device leave again to finish it. Use --verbose for details.\n");
        output::Tone::Attention
    };
    for failure in &not_removed {
        text.push_str(&format!(
            "{:?} could not be removed ({}). Run koi device leave again to remove it.\n",
            failure.path,
            printable(&failure.error),
        ));
    }
    let data = LeftData {
        not_removed: (not_removed.into_iter())
            .map(|failure| NotRemovedData {
                path: failure.path.to_string_lossy().into_owned(),
                error: failure.error,
            })
            .collect(),
        unconfirmed: unconfirmed.map(|unconfirmed| UnconfirmedData {
            server: unconfirmed.server,
            device_id: unconfirmed.device_id,
            reason: unconfirmed.reason,
        }),
        set_aside,
    };
    Done {
        tone,
        ..Done::ok(data, text).human(summary, "")
    }
}

/// Runs a device command (RFC 0009).
pub(crate) async fn device(paths: Paths, action: DeviceAction) -> Result<Done, CliError> {
    match action {
        DeviceAction::Leave { last_device } => {
            let request = Request::Leave {
                confirm_last_device: last_device,
            };
            match ipc::run(&paths, request).await? {
                Response::SignedOut {
                    not_removed,
                    unconfirmed,
                    set_aside,
                } => Ok(left(not_removed, unconfirmed, &set_aside)),
                other => Err(unexpected(other)),
            }
        }
        DeviceAction::List => {
            let auth = signed_in(&paths)?;
            let devices = devices(&auth).await?;
            let data = DevicesData {
                devices: devices.iter().map(DeviceData::of).collect(),
            };
            Ok(Done::ok(data, device_table(&devices, now_ms())))
        }
        DeviceAction::Invite { request } => {
            let auth = signed_in(&paths)?;
            let claim = match request.request_id {
                None => None,
                Some(id) => {
                    let key = Key::new(
                        "device invite",
                        serde_json::json!({
                            "server": auth.api().base().to_string(),
                            "device_id": auth.credentials().device_id,
                        }),
                    );
                    match request::begin(&paths, &id, &key)? {
                        Begun::Done(recorded) => {
                            let invitation = serde_json::from_value(recorded).map_err(|error| {
                                format!("the record of the request {id} cannot be read: {error}")
                            })?;
                            return replayed(&id, &invitation, now_ms());
                        }
                        Begun::Claimed(claim) => Some(claim),
                    }
                }
            };
            let started = match auth.api().pair_start(&auth).await {
                Ok(started) => started,
                Err(error) => {
                    if let Some(claim) = claim.filter(|_| refused(&error)) {
                        claim.abandon();
                    }
                    return Err(error.into());
                }
            };
            let invitation = Invitation {
                code: started.code,
                server: auth.api().base().to_string(),
                expires_at_ms: started.expires_at_ms,
            };
            if let Some(claim) = claim {
                claim.finish(&invitation)?;
            }
            invited(&invitation, now_ms())
        }
        DeviceAction::Revoke { id } => {
            let auth = signed_in(&paths)?;
            if id == auth.credentials().device_id {
                return Err(CliError::usage(
                    "this device cannot revoke itself; revoke it from another device",
                ));
            }
            let revoked = devices(&auth).await.is_ok_and(|devices| {
                (devices.iter()).any(|device| device.device_id == id && device.revoked_at_ms != 0)
            });
            if revoked {
                let text = format!("Device {id} is already revoked.\n");
                return Ok(Done::ok(RevokedData { device_id: id }, text).unchanged());
            }
            auth.api().revoke_device(&auth, &id).await?;
            let text = format!(
                "Revoked device {id}. It can no longer sync; the files already on it stay there.\n"
            );
            Ok(Done::ok(RevokedData { device_id: id }, text).completed())
        }
    }
}

/// Whether the call changed nothing on the server: it was never sent, or the server refused it with a 4xx status. Without
/// an answer, or with a 5xx status, whether or not its error could be read, the call may have changed something.
pub(crate) fn refused(error: &ApiError) -> bool {
    match error {
        ApiError::Url(_) | ApiError::PathSegment(_) | ApiError::Credential(_) => true,
        ApiError::Server { status, .. }
        | ApiError::Status { status }
        | ApiError::Unreadable { status } => (400..500).contains(status),
        ApiError::Transport(_) | ApiError::TooLarge { .. } | ApiError::Decode(_) => false,
    }
}

/// Every device of the account, page by page. A page that repeats an earlier cursor ends the listing as undecodable, as
/// in `Api::list_spaces`.
pub(crate) async fn devices(auth: &Auth) -> Result<Vec<DeviceInfo>, CliError> {
    auth.api()
        .all_devices(auth)
        .await
        .map_err(CliError::api_retry_safe)
}

/// A pairing code `koi device invite` showed, as its request record keeps it.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Invitation {
    pub(crate) code: String,
    pub(crate) server: String,
    pub(crate) expires_at_ms: i64,
}

/// What `koi device invite` shows for `invitation`: the code on stdout, how to use it on stderr.
pub(crate) fn invited(invitation: &Invitation, now_ms: i64) -> Result<Done, CliError> {
    let server = url::Url::parse(&invitation.server).map_err(|error| error.to_string())?;
    let minutes = ((invitation.expires_at_ms - now_ms + 59_999) / 60_000).max(1);
    let code = pairing::join_args(&invitation.code, &server);
    let note = format!(
        "On the new device, run:\n  koi join {code}\nThe code works once, for about {minutes} minutes.\n"
    );
    let mut join_argv = vec!["koi".to_owned(), "join".to_owned()];
    join_argv.extend(pairing::join_argv(&invitation.code, &server));
    let data = InvitationData {
        join_argv,
        expires_at_ms: invitation.expires_at_ms,
    };
    Ok(Done::value(data, &code, note))
}

/// The invitation the request `id` showed before, or a stale request once it expired.
pub(crate) fn replayed(
    id: &RequestId,
    invitation: &Invitation,
    now_ms: i64,
) -> Result<Done, CliError> {
    if invitation.expires_at_ms <= now_ms {
        return Err(CliError::new(
            Code::StaleDecision,
            format!(
                "stale request: the pairing code of the request {id} expired; run koi device invite with a new \
                 request ID"
            ),
        ));
    }
    invited(invitation, now_ms)
}

/// One line per device: its ID, name, platform, when it last refreshed a token, and whether it is this device, active
/// or revoked.
pub(crate) fn device_table(devices: &[DeviceInfo], now_ms: i64) -> String {
    let rows: Vec<[String; 5]> = devices
        .iter()
        .map(|device| {
            let state = match (device.revoked_at_ms, device.current) {
                (0, true) => "this device".to_owned(),
                (0, false) => "active".to_owned(),
                (revoked, _) => format!("revoked {}", ago(revoked, now_ms)),
            };
            [
                printable(&device.device_id),
                printable(&device.name),
                printable(&device.platform),
                ago(device.last_seen_ms, now_ms),
                state,
            ]
        })
        .collect();
    let header = ["ID", "NAME", "PLATFORM", "LAST SEEN", "STATE"].map(str::to_owned);
    let mut widths = [0; 5];
    for row in std::iter::once(&header).chain(&rows) {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut table = String::new();
    for row in std::iter::once(&header).chain(&rows) {
        let cells: Vec<String> = row
            .iter()
            .zip(widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        table.push_str(cells.join("  ").trim_end());
        table.push('\n');
    }
    table
}
