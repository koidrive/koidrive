// SPDX-License-Identifier: Apache-2.0

use crate::telemetry;
use koidrive_ipc::RequestId;

/// `--request-id`, for a change an agent may run again after a timeout.
#[derive(clap::Args)]
pub struct RequestArg {
    /// An ID you pick for this change, of letters, digits, - and _. Running the command again with the same ID and
    /// arguments returns the first outcome and changes nothing again.
    #[arg(long, value_name = "ID", value_parser = RequestId::parse)]
    pub request_id: Option<RequestId>,
}

#[derive(clap::Args)]
pub struct DeviceArgs {
    /// A name for this device, shown on the others, at most 255 bytes. Defaults to this machine's host name.
    #[arg(long, value_parser = short_name)]
    pub(crate) name: Option<String>,
    /// Turn usage reports on or off for this machine, without being asked (see koi telemetry).
    #[arg(long, value_enum)]
    pub(crate) telemetry: Option<telemetry::Switch>,
}

impl DeviceArgs {
    pub fn name(&self) -> String {
        self.name.clone().unwrap_or_else(|| {
            let host = rustix::system::uname()
                .nodename()
                .to_string_lossy()
                .into_owned();
            host.split('.').next().unwrap_or_default().to_owned()
        })
    }
}

/// A space, device or user name as the server takes it: at most 255 bytes of UTF-8, measured before trimming (RFC 0007
/// section 1). Checking it here names the argument instead of sending a request the server refuses.
pub(crate) fn short_name(text: &str) -> Result<String, String> {
    const MAX_NAME: usize = 255;
    if text.len() > MAX_NAME {
        return Err(format!(
            "a name is at most {MAX_NAME} bytes of UTF-8, and this one has {}",
            text.len()
        ));
    }
    Ok(text.to_owned())
}
