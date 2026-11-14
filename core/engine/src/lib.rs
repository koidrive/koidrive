// SPDX-License-Identifier: Apache-2.0

//! Sync state machine and conflict rules, free of I/O.

pub mod authors;
mod casefold_data;
pub mod classify;
pub mod commit;
pub mod effects;
pub mod first_link;
pub mod guard;
pub mod ids;
pub mod ignore;
pub mod names;
pub mod question;
mod recovery;
pub mod restore;
pub mod state;
pub mod tracked;
pub mod tree;
