# RFC 0026: Edits carry the versions they were made over

Status: proposed. Applies to spec v0, format_version 1. Amends spec v0 §6.2 R3, RFC 0016, RFC 0018, RFC 0004 §5, and RFC 0006 §8.

## 1. Messages

`UpdateContent` adds `repeated string made_over = 4` and `string source_version_id = 5`. `CreateFile` adds `repeated string made_over = 5` and `string source_version_id = 6`. `VersionInfo` adds `repeated string made_over = 7`, including the current version in `NodeState`, change records, version listings, and exports.

An operation's `made_over` identifies versions whose content the edit was made over. Clients MUST order these IDs newest first and MUST send at most 64 distinct IDs. IDs MUST be UUIDv7 version IDs. An ID does not assert that the server still retains that version.

`source_version_id` identifies the value a resend or re-create carries, independently of its `made_over` bases. Equal content does not identify a version. A resend SHOULD set it to the ID of the version whose content it sends again. A `CreateFile` that re-creates a known value after node loss SHOULD set it too. The server MUST NOT interpret it as a base or infer it from content. It has no effect on an `UpdateContent` without `resend`.

## 2. Stored ancestry

The server MUST expand the incoming `made_over` before deciding R3. It visits the incoming IDs in order, retaining the first occurrence of each ID. Immediately after retaining an ID, it visits that stored version's `made_over` in the same way, but only if the version belongs to the target node. Unknown IDs and IDs of another node remain in the result without expansion. An ID already visited MUST NOT be expanded again. The walk stops after retaining 64 IDs.

This depth-first order defines the expanded bases without comparing version timestamps. When a resend or `CreateFile` creates a version, including a conflict copy or renamed create, the server MUST prepend its nonempty `source_version_id` to those bases, remove later duplicates, and truncate the result to 64 IDs. An `UpdateContent` without `resend` MUST NOT add a source alias. The server MUST persist the resulting chain with the new version.

The source entry is a same-value alias, not evidence of a user's overwrite. A replay mints a new server version ID for the value identified by `source_version_id`. `VersionInfo.made_over` reports both such aliases and overwritten bases as inclusion evidence. An edit over that replay therefore includes its original source value when the server expands the replay's chain. Adding an alias MUST NOT change RFC 0018's edit attribution or make a resend count as a user's edit.

A chain MUST contain no more than 64 distinct IDs in storage or any response, including source aliases. Repeated edits MUST NOT increase that bound. An upgrade MUST give existing versions an empty chain. An ID outside the retained chain supplies no inclusion evidence, so the existing conflict rules apply when the relationship is unknown.

## 3. Applying edits and resends

If the expanded bases contain the target's current version ID, R3 MUST NOT make a conflict copy for an unseen content change. A non-resend edit replaces that version as an ordinary user overwrite. The resend's source alias alone MUST NOT satisfy this condition. The server MUST NOT infer this relationship from `base_seq`, version age, or equal content.

Before applying a resend, if the target's current stored chain contains its nonempty `source_version_id`, the server MUST return `SUPERSEDED` and change nothing. The current version already includes the resend's source value, either as the same value or through an edit made over it. The server MUST NOT add incoming bases or aliases to the unchanged version on this path. RFC 0018's `SUPERSEDED` result for a trashed target still applies.

When a resend with a nonempty source ID meets unseen equal content under RFC 0016, the server MUST merge that source alias into the current version's chain, source first, with first-occurrence deduplication and the same 64-ID bound. If this adds knowledge, the result and change record MUST report the updated `NodeState`. This makes no version, moves no `content_rev` or `edit_rev`, and does not revive a trashed file. Incoming bases MUST NOT rewrite the equal version's chain. Without a source ID, the existing equal-content behavior remains unchanged.

`CreateFile` MUST persist its expanded ancestry and source alias, including when a name collision renames the new file. It MUST NOT use them to replace another node or bypass existing create checks. This preserves both a user's edit made over a lost value and a resend re-created after node loss. An initial create or restore without ancestry creates a version with an empty chain. All other rules remain unchanged, including RFC 0018's edit attribution and revival.

## 4. Export replay

An alias-only record can update a version's chain without advancing the node's content, edit, or metadata revisions. Before RFC 0004 §5's node-revision comparison, a reader MUST apply the record's bounded chain to a matching current or retained version ID. With no snapshot, or for a record after the snapshot manifest's `end_seq`, the incoming chain replaces the known chain. For a record within a fuzzy snapshot's `start_seq` to `end_seq` interval, the reader MUST keep the known chain first, append incoming IDs, deduplicate, and truncate to 64. This retains valid inclusion evidence without replacing a newer snapshot chain with an older record's chain.

## 5. Digest compatibility

Commit request digest v4 is v3's projection plus both new fields on `UpdateContent` and `CreateFile`, preserving the request's order and duplicates before expansion. The server MUST store v4 with new commit results. A reused `commit_id` with different ancestry fields MUST be `INVALID_ARGUMENT` when its stored digest is v4. For a stored v1, v2, or v3 result, the server MUST reject a retry with any nonempty ancestry or source field rather than hash those fields away. Retries with empty new fields MUST continue to use their original digest projections. This applies to both the commit log and the device's retained result after log compaction. Empty ancestry fields do not change the original projections' encoded bytes.

## 6. Mixed versions

An old client sends neither field. A new server MUST retain the existing conflict-copy behavior for that client's edits, even when the server stores ancestry on the current version. A resend without `source_version_id` MUST NOT be superseded by ancestry. Existing equal-content and trash rules still apply.

An old server ignores both fields and reports no ancestry. A new client MUST treat absent ancestry as empty and MUST NOT infer that a later edit includes a source value. It retains the existing resend and conflict-copy behavior in that direction.

## 7. Conformance

Server tests cover an edit over an unseen current version, expansion through a retained version after a rollback, an overwritten resend returning `SUPERSEDED` without a write, an old client retaining a distinct write as a conflict copy, digest versions, and a sequence longer than 64 edits whose stored versions, change records, and listings remain bounded.
