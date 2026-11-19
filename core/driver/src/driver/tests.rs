// SPDX-License-Identifier: Apache-2.0

include!("tests/claims.rs");
include!("tests/common.rs");
include!("tests/credentials.rs");
include!("tests/failed_syncs.rs");
include!("tests/gitignore.rs");
include!("tests/halts_and_damaged_stores.rs");
include!("tests/links_and_unlinks.rs");
include!("tests/pause_and_watchers.rs");
include!("tests/profile_and_questions.rs");
include!("tests/restores.rs");
include!("tests/revocation.rs");
include!("tests/running_folders.rs");
include!("tests/sign_out.rs");
include!("tests/sign_out_cleanup.rs");

use crate::failures::{self, Failure};
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

use crate::leave::Intent;
use crate::question::{HeldItem, QuestionKind};
use crate::restore::{RestoreRejection, RestoreState, RestoreWait};

mod root_token;
mod stop_during_open;

use super::link::{
    UNLINK_BROUGHT_BACK, held_aside, record_unreported_set_aside, recorded_brought_back,
    resolve_space,
};
use super::questions::decode_question;
use super::*;
