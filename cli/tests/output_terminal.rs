// SPDX-License-Identifier: Apache-2.0

include!("output_terminal/common.rs");
include!("output_terminal/error.rs");
include!("output_terminal/render.rs");
include!("output_terminal/status.rs");

#[path = "support/terminal.rs"]
mod terminal;

use terminal::Terminal;

#[path = "support/process.rs"]
mod process;
