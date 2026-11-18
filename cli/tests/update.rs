// SPDX-License-Identifier: Apache-2.0

include!("update/app.rs");
include!("update/common.rs");
include!("update/homebrew.rs");
include!("update/installer.rs");
include!("update/status.rs");

mod schema;

#[path = "support/executable.rs"]
mod fixture;

#[path = "support/terminal.rs"]
mod terminal;

use terminal::Terminal;
