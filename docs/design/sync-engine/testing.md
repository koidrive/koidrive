# 7. Testing

The tests below check the rules of this reference. [Testing](../../testing.md) says how to run them and how the property battery and mutation set run on GitHub Actions.

## 7.1 Reference server

`core/sim/src/space` is an in-memory Space DO written from spec §6 and §8: commit idempotency, `base_seq` checks, chunk presence, rules R1 to R11 with the unseen-change definition, trash groups, revs and post-image change records. It is the server for every engine test. `core/sim/tests/space_rules.rs` and `space_properties.rs` test its rules. `core/sim/tests/conformance.rs` and `server/test/conformance.test.ts` run the shared scenario files in `spec/testdata/scenarios/` against the reference server and the TypeScript server.

## 7.2 Property tests

- Chunking (`core/chunk`): chunks concatenate to the input, sizes stay within bounds, boundaries are deterministic, and the manifest digest matches spec §5.1.
- Names (`core/engine/tests/names.rs`): `name_key` and normalization are idempotent, canonically equivalent names share a key, and conflict names are valid and distinct.
- Trees (`core/engine/tests/trees.rs`): settling a change both sides made never orphans a node or gives two siblings one name key in S.
- Planner (`core/sim/tests/engine_sync.rs`): a device with offline local changes meets another device's concurrent commits and plans until nothing is left. It must end with L = S = R equal to the server's tree, with every write on the server, every restore request ended with the server's result and sent once, and no `Delete` or removal caused by a rule change.

## 7.3 Deterministic simulation

`core/sim/src/world` runs whole devices against the reference server in one thread. Each device has a disk, an engine, the state its store holds and a connection. A schedule names one step at a time: a user action, a rescan, an effect step, a commit, the delivery of any one message, or a fault. Faults include lost, repeated and reordered messages, refused commits, crashes between store transactions and inside an effect, server restores that lose commits, rewinds, and store rollbacks. After the schedule the faults stop and the devices run until nothing is left.

The oracle uses the harness's own records and the server's state, never the engine's bookkeeping. Every user write carries a tag, so two files with the same bytes are two writes. A run fails when a write is lost (§3.3), when a commit takes a write out of the server's tree without a user's overwrite or delete, when L, S or R stop being valid trees, or when the engine, the store or the server refuses a state.

`core/sim/tests/devices.rs` runs these schedules as proptest properties: `devices_lose_no_write` and `devices_converge_once_the_faults_stop`, each also across server restores and across rewinds and rollbacks. `devices_converge_across_server_restores` is ignored as a known failure: after a server loss, a device can move a node back to a position it never set. Some failures count as divergences rather than lost writes, such as a write the engine trashed without its user deleting it. The `devices_lose_no_write` properties stop only on a lost write or a refused state, and the convergence properties also check divergences.

## 7.4 Native contract tests

`core/fs/tests` tests the effect protocol of §5.2 and recovery of §5.3 on the file system the tests run on. Each test in `effect.rs` and `crash.rs` acts at one hook point between the steps of §5.2:

- a user write between the check and the quarantine, between the quarantine and the placement, or through a descriptor held across the effect;
- a parent replaced by a link or another folder, or moved out of the root;
- the target name taken during placement;
- platform files in a folder to remove;
- a crash at a hook, followed by recovery.

Each test asserts that every byte the test wrote within the supported-writer contract still exists, at the target, in the trash or as a recovered file. The tests that move a parent out of the root or replace it with a link also check that nothing outside the root was touched. `trash.rs` covers reclamation by age and size, `root.rs` the root check and the no-replace probe, and `ignore.rs` the walk of §5.10. Power-loss durability is not tested.

## 7.5 Named scenarios

Each scenario is a fixed schedule and the outcome that must hold after it. The scenarios marked *recovery* check §6.4 and must pass before 0.1.0. A failure there changes §6.4, not the scenario. The others check the core of the design.

| Scenario | Schedule | Must hold |
|---|---|---|
| `exchange-crash` | A and B synced; an incoming change replaces A; the user writes C between the check and the quarantine step; the device crashes before the comparison and restarts | C is recovered to its name or kept in quarantine; nothing deletes it; the incoming version is not adopted over it |
| `replace-races` | the user writes the target between each pair of steps of §5.2 | the newer bytes are never overwritten: they stay in place or reach the local trash. Open descriptors are native-only (§7.4) |
| `pitr-replay` | Delete K applies at seq 100 with base 99 and its result is lost; the server is restored to seq 99 with a new history; the device reconnects | K is never resent or replayed; the fence refuses it; recovery starts (*recovery*) |
| `rename-after-restore` | a synced node is renamed from /a to /b after the restore point, and another file is created after it; the space is rewound | one node, moved back to /a, with no duplicate; the new file is held, never uploaded unanswered (*recovery*) |
| `ledger-restart` | in a large space, a device deletes 900 files, restarts, and deletes 900 more within the window | the second batch reaches the guard; ledger, holds and approvals survive the restart |
| `oracle-mutants` | five planted bugs: an fs driver that skips the quarantine comparison (a lost user write), one that deduplicates files by content digest (a lost duplicate), one that sweeps quarantine at startup (an interrupted replace), an engine that re-sends an old edit of a trashed node (a false resurrection), and one that drops a conflict copy once its original is overwritten (a lost surviving branch); plus identical bytes written to two files and one deleted | each planted bug fails some bounded seed and fails again on rerun; the surviving copy is still required. `scripts/engine-mutations.sh` mutates engine code separately and does not plant these bugs |
| `purge-then-old-state` | a `NodePurged` record followed by a redelivered older `NodeState`; an own result whose post-image names a parent created by the previous commit | the purged node is not recreated; R stays a valid tree |
| `lost-result-then-delete` | A writes B over A; K applies at seq 11 and its result is lost; another device deletes n at seq 12 after seeing B; the log is truncated past K; the device reconnects | K is answered by its stored result; n stays trashed |
| `result-after-replay` | a log replay delivers the device's own commit before its `CommitResult` | the result is acknowledged at once; the commit leaves flight; no metadata is replayed twice |
| `late-descriptor-write` | a write through a descriptor opened before a replacement lands after the trash entry expires or is evicted by the cap | outside the contract of §5.2. The native tests of §7.4 check the other side of the expiry and cap-eviction cuts: a write before reclaim is recovered |
| `move-into-ignored` | a synced file is moved into an ignored tree while events are lost; separately, a remote move targets a path ignored on this device | no `Delete` is sent; the local copy is not trashed |
| `conflict-branch-survives` | n is overwritten after a conflict copy Y split off from it; a commit is retried after its response was lost | Y's write is still required; the retry consumes no new intent |
| `stale-store-replay` | K applies and is acknowledged; J is accepted; K falls below the log floor; the device restarts from a store copied before K's acknowledgement and retries K | `STALE_DEVICE_STATE`; recovery starts; nothing deleted after K is revived (*recovery*) |
| `ignored-atomic-save` | synced /f is moved into an ignored tree while events are lost, then saved by temp-file-and-rename; in a variant the ignored tree cannot be listed | no `Delete`; the disappearance is held for the user |
| `unpublished-cancel` | offline, a file is created, overwritten and deleted before any commit; a temporary file lives between two rescans | no op is sent and the oracle requires none |
| `accepted-response-lost` | with *a* = 4, commit K takes slot 5 and is accepted; its response is lost; `Welcome` reports 5 | K is resent and answered by its stored result; no recovery starts (*recovery* trigger) |
| `refused-then-replan` | K takes slot 5 and is refused with `MISSING_CHUNKS` because the file changed during upload; the engine re-plans | the new commit takes slot 5 with a new `commit_id` and is accepted (*recovery* trigger) |
| `held-node-published-elsewhere` | device X holds n = B in recovery; device Y answers its own question and publishes n = C; variants B ≠ C, B = C at another position, B = C at the same position | the first two stay held on X with refreshed questions; only the last is released (*recovery*) |
| `restore-intent-in-flight` | with L = S = B, an in-flight `Restore(n, version C)`, and in a variant a trash restore, has an unknown outcome when the history changes; R′ holds B | the restore is offered as a question; recovery does not end and the evidence is kept until it is answered (*recovery*) |
| `history-change-on-connect` | a device reconnects with no commit in flight and `Welcome` reports a new `history_id` | no `ListTree` bootstrap replaces R; recovery starts with R, S and L kept as evidence |
| `delete-folder-with-excluded-child` | synced folder D holds a synced file and a file excluded here by a rule; the user removes D locally | only the synced file is deleted on the server; D and the excluded file stay there and on other devices; D is not created again here; the guard counts one deletion |
| `move-out-of-exclusion` | a synced file becomes excluded by a new rule, and the user moves it to an included path; in a variant a remote move first took it into a path ignored here | the move reaches the server and the file syncs again; an unmoved local copy stays where it is |
| `question-after-restart` | the device crashes after the transaction that stores a guard hold and before `AskUser` reaches the user | after restart the question is asked again; the held deletions are neither committed nor lost |
| `answer-concurrent-change` | between recovery's simulation of an answer and its commit, another device changes or moves a descendant that the answer's R8 revival would touch | the precondition refuses the commit with nothing applied; the question is asked again against the new R′ (*recovery*) |
| `offline-delete-no-user-ignores` | with an empty `.koiignore`, a synced file is deleted while the engine is not running; the rescan cannot attribute it to a rename | a normal deletion through the guard; `.koidrive/` alone never makes it ambiguous |
| `remote-rename-then-store-rollback` | with no commit of its own, a device applies a remote rename of n from /a to /b to its disk, then restarts from a store copied before that; in a variant, another device deletes n after the rename | the effect counter starts recovery; n stays one node at /b and no new file is uploaded; in the variant n stays trashed and no R8 revival undoes the delete (*recovery*) |
| `rewound-write-obligation` | a write is published after the restore point and the space is rewound; the answer is Publish, Keep or Discard, chosen by the seed | until the answer, the held copy keeps its bytes and the server is not required to hold the write; after Publish or Keep it is held by the server and every eligible device; after Discard it is in the local trash (*recovery*) |
| `answer-revives-trash-group` | S₀ has directory D with child F; the user moves D and deletes F; R′ has D and F trashed in one trash group; the user publishes D | F stays trashed, or is listed in the question before anything is sent; no node outside the answer changes (*recovery*) |
| `lost-edit-downloaded-elsewhere` | A edits f and B takes the edit; the server is restored to before it; A and B list the restored tree, then either commits first; in a variant C edits f after the loss, before A and B reconnect | f ends at A's content on every device with no conflict copy; in the variant C's edit stays at f and A's content is kept beside it |

These tests run named scenarios today:

- `core/sim/tests/recovery.rs` covers the recovery triggers of §2.8 and §6.4: `pitr_replay` (a commit whose result was lost is never resent into the rewound history), `history_change_on_connect` (no bootstrap replaces R), `stale_store_replay` (`STALE_DEVICE_STATE` refuses the copied store's commit, recovery starts, and both files stay live), `store_rollback_below_the_log_floor` (a store rolled back below the log floor, with no commit of its own since the copy, resyncs without starting recovery and keeps the other device's new file), `remote_rename_then_store_rollback` (the effect counter starts recovery), `accepted_response_lost` and `refused_then_replan` (the device counter's lifecycle), the `rewound_write_*` tests (a rewound write is held, never uploaded unanswered, and keeps its bytes), `a_recovery_answer_survives_a_crash` and `a_keep_given_while_the_device_is_down_counts_after_the_restart` (the outcomes after Discard and Keep, across a crash), and `an_overwrite_after_a_keep_through_a_rewind_replaces_the_kept_write_on_every_device` (an ordinary overwrite replaces a kept write).
- `core/engine/tests/recovery.rs` runs `answer-concurrent-change` against the reference server in `a_published_folder_whose_child_another_device_moves_before_the_commit_is_asked_again_and_changes_only_itself`.
- `core/sim/tests/devices.rs` holds fixed schedules for results replayed before they arrive, crashes inside downloads and replacements, conflict copies, pending names, and commits the server lost (§4.4).
- `core/engine/tests` covers single rules: `guard_model.rs` (§6.1), `ignore.rs` (§5.10), `restore.rs` (§6.3), `first_link.rs` (§6.4), `state.rs` (§2.8), and `commit.rs` and `effects.rs` (§2.5 to §2.7).
