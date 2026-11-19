# SPDX-License-Identifier: Apache-2.0
# shellcheck shell=bash disable=SC2034

unit=(cargo test -q --locked -p koidrive-engine --test commit)
property=(cargo test -q --locked -p koidrive-sim --test engine_sync)
devices=(cargo test -q --locked -p koidrive-sim --test devices)

C=core/engine/src/commit.rs
E=core/engine/src/effects.rs
L=core/engine/src/classify.rs

mutation agreed-batch $C 's/    \(needed, agreed\)\n\}/    let _ = agreed;\n    (needed, Vec::new())\n}/' prop
mutation merge-delete-wait $C 's/scratch\.protected\.insert\(level\.clone\(\)\);/let _ = \&level;/' both
mutation pending-name $C 's/\n            && \(scratch\.holders\(&shape\.parent, &shape\.name\)\)\.any\(\|other\| \{\n.*?\n            \}\)\n        \{/\n            \&\& false\n        {/s' both
mutation renamed-revert $C 's/\(renamed && !left && !kept\)/(false \&\& renamed \&\& !left \&\& !kept)/; s/else if renamed && !left/else if false \&\& renamed \&\& !left/' both
mutation merged-revert $C 's/trees\.revert\(create_id\.clone\(\)\);\n//' both
mutation cycle-revert $C 's/        for id in reverts \{\n            trees\.revert\(id\);/        for id in reverts {\n            let _ = id;/' both
mutation split-halves $C 's/self\.cap\.set\(Some\(ops\.len\(\) \/ 2\)\);/self.cap.set(Some(ops.len()));/' both
mutation split-delete $C 's/self\.split\.insert\(NodeId::from\(delete\.node_id\)\);/let _ = delete;/' unit
mutation cap-regrowth $C 's/self\.cap\.map\(\|cap\| cap \* 2\)/self.cap.map(|cap| cap)/' unit
mutation byte-budget $C 's/\(emitted == 0 \|\| bytes \+ cost <= MAX_COMMIT_BYTES\)/(emitted == 0 || bytes + cost <= usize::MAX - cost)/' unit
mutation large-delete $C 's/if kept \|\| opened \|\| bytes > MAX_COMMIT_BYTES \|\| split\.contains\(id\)/if kept || opened || split.contains(id)/' unit
mutation device-seq $C 's/device_seq: \*self\.accepted \+ 1,/device_seq: *self.accepted,/' unit
mutation accepted $C 's/        self\.accepted\.set\(flight\.commit\.device_seq\);\n//' unit
mutation limit-split $C 's/Ok\(Code::LimitExceeded\) => Failure::Split,/Ok(Code::LimitExceeded) => Failure::Abandoned,/' both
mutation alias-plan $C 's/let local = match aliases\.get\(&local\.parent\)/let local = match None::<\&NodeId>/' both
mutation revert-no-rewrite $E 's/!same_content\(old, new\) && remote_only\(self\.trees, id\)/!same_content(old, new)/' both
mutation reverted-candidate $E 's/\n        \|\| \(trees\.reverted\(\)\.contains\(id\)\n            && \(trees\.local\(\)\.get\(id\)\.is_some\(\)\n                \|\| holds_below\(trees, id\)\n                \|\| arrives_below\(trees, id\)\n                \|\| trees\.kept_for_excluded\(\)\(id\)\)\n            && \(trees\.remote\(\)\.live\(id\)\.is_some\(\) \|\| !trees\.edited\(id\)\)\)//' both
mutation reverted-edit-guard $E 's/\n            && \(trees\.remote\(\)\.live\(id\)\.is_some\(\) \|\| !trees\.edited\(id\)\)\)/)/' both
mutation reverted-delete $C 's/&& !needed\.contains\(id\)\n\}/\&\& !needed.contains(id)\n        \&\& !trees.reverted().contains(id)\n}/' both
mutation reverted-no-create $C 's/if !trees\.reverted\(\)\.contains\(id\) && !trees\.is_discarded\(id\)/if !trees.is_discarded(id)/' both
mutation reverted-no-move $C 's/\n            && !trees\.reverted\(\)\.contains\(id\)\n            && !trees\.reviving\(id\)/\n            \&\& !trees.reviving(id)/' both
mutation reverted-parking $E 's/\(candidate\(trees, id\) \|\| trees\.waiting\(\)\.contains_key\(id\) \|\| trees\.reclaimed\(id\)\)/(trees.acknowledged_class(id) == Class::Remote || trees.waiting().contains_key(id) || trees.reclaimed(id))/' both
mutation pending-truncate $C 's/while nfc\(&named\(&stem\)\)\.len\(\) > MAX_NAME_BYTES/while false \&\& nfc(\&named(\&stem)).len() > MAX_NAME_BYTES/' both
mutation keep-parked $L 's/\.is_some_and\(\|node\| node\.parked \|\| self\.reverted\.contains\(id\)\)/.is_some_and(|_| self.reverted.contains(id))/' both
mutation parked-reverted-remove $E 's/!\(self\.trees\.reverted\(\)\.contains\(id\)\n                        \|\| self\.trees\.waiting\(\)\.contains_key\(id\)\)/!self.trees.waiting().contains_key(id)/' both
mutation parked-reverted-edit $E 's/ \|\| self\.trees\.edited\(id\)\) =>/) =>/' both
mutation reverted-retain $L 's/\(local, remote\) => local\.is_some\(\) \|\| remote\.is_some\(\),/(local, _) => local.is_some(),/' both
mutation alias-in-place $C 's/if in_alias && \(placed\(agreed\) \|\| placed\(remote\)\) && !trees\.reverted\(\)\.contains\(id\) \{\n            reverts\.push\(id\.clone\(\)\);/if in_alias \&\& (placed(agreed) || placed(remote)) \&\& !trees.reverted().contains(id) {\n            let _ = id;/' both
mutation rekey-edit $C 's/            && !trees\.held\(id\)\n        \{\n            rekey_copy\(trees, id, &NodeId::from\(ids\.uuid_v7\(now_ms\)\)\);/            \&\& !trees.held(id)\n        {\n            let _ = (\&ids, now_ms);/' both
mutation keep-revert-blocked $C 's/if !trees\.set_synced_if_valid\(node\.clone\(\), None\) \{\n            trees\.revert\(node\.clone\(\)\);/if !trees.set_synced_if_valid(node.clone(), None) {/' both
mutation quota-retry $C 's/            Ok\(Code::QuotaExceeded\) if error\.retry_after_ms > 0 => Failure::Resend,\n//' unit
mutation refused-reconnect $C 's/_ if policy && w >= a => return Reconnect::Idle,/_ if policy \&\& w >= a => return Reconnect::Resend,/' unit
mutation unknown-resend $C 's/            _ => Failure::Halted\(Halt::Policy \{ code: error\.code \}\),/            Err(_) => Failure::Resend,\n            _ => Failure::Halted(Halt::Policy { code: error.code }),/' unit
mutation parked-name $L 's/self\.parked_as\.get\(id\)\.unwrap_or\(id\)/{ let _ = \&self.parked_as; id }/' unit
mutation rekey-park-as $C 's/        trees\.park_as\(to\.to_owned\(\), parked_name\);\n//' unit
mutation digest-dedupe $E 's/    let changed = remote_only\(trees, id\)\n/    let changed = trees.local().get(id).is_some_and(|node| {\n        let digest = |node: \&LocalNode| node.shape.content.as_ref().map(|content| content.manifest_digest.clone());\n        let twin = |(other, twin): (\&NodeId, \&LocalNode)| other < id \&\& digest(twin) == digest(node);\n        digest(node).is_some() \&\& trees.synced().live(id).is_none() \&\& trees.remote().live(id).is_none() \&\& trees.local().iter().any(twin)\n    }) || remote_only(trees, id)\n/' devices
