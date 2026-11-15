// SPDX-License-Identifier: Apache-2.0

use crate::error::CliError;
use crate::profile::unexpected;
use crate::report::{Done, Empty};
use crate::{ipc, output};
use koidrive_driver::Paths;
use koidrive_ipc::{Request, Response};

pub(crate) async fn pause(paths: &Paths) -> Result<Done, CliError> {
    match ipc::run(paths, Request::Pause).await? {
        Response::Paused => Ok(Done {
            tone: output::Tone::Completed,
            ..Done::ok(Empty {}, "Paused sync. To sync again, run: koi resume\n")
        }),
        Response::AlreadyPaused => Ok(Done::ok(Empty {}, "Sync is already paused.\n").unchanged()),
        other => Err(unexpected(other)),
    }
}

pub(crate) async fn resume(paths: &Paths) -> Result<Done, CliError> {
    match ipc::run(paths, Request::Resume).await? {
        Response::Resumed => Ok(Done {
            tone: output::Tone::Completed,
            ..Done::ok(Empty {}, "Resumed sync.\n")
        }),
        Response::NotPaused => Ok(Done::ok(Empty {}, "Sync is not paused.\n").unchanged()),
        other => Err(unexpected(other)),
    }
}
