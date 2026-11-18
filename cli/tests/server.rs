// SPDX-License-Identifier: Apache-2.0

//! The `koi` command against the real Worker in `server/`, run by `cli/scripts/integration-test.sh`, which starts
//! `wrangler dev` and sets `KOIDRIVE_TEST_SERVER` (its URL) and `KOIDRIVE_TEST_SERVER_LOG` (its output, where the
//! Worker prints the claim code). The owner's device is driven through `koidrive-net`; the device under test only
//! through the command.

include!("server/common.rs");
include!("server/device.rs");
include!("server/history.rs");
include!("server/service.rs");
include!("server/share.rs");
include!("server/space.rs");

mod schema;

#[cfg(target_os = "linux")]
#[path = "support/executable.rs"]
mod fixture;

#[path = "support/process.rs"]
mod process;
