// SPDX-License-Identifier: Apache-2.0
#![allow(dead_code)]

pub const KOI: &str = env!("CARGO_BIN_EXE_koi");

pub fn koi(within: &std::path::Path) -> std::process::Command {
    koidrive_test_support::sandboxed(KOI, within)
}
