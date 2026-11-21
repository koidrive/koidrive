// SPDX-License-Identifier: Apache-2.0

#![allow(dead_code)]

const FIELD_INTENTS_BY_DEFAULT: bool = true;

include!("engine_sync/common.rs");
include!("engine_sync/first_link.rs");
include!("engine_sync/held_renames_and_purged_folders.rs");
include!("engine_sync/moves_and_deletes.rs");
include!("engine_sync/parked_nodes.rs");
include!("engine_sync/restores.rs");
include!("engine_sync/rules_and_held_entries.rs");
include!("engine_sync/waiting_updates.rs");
