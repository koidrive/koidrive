# 9. What the other crates provide

- `core/fs`: snapshots and typed changes as the observation source, with `Content` from `core/chunk`, a file identity for every entry, the root check (§6.2) with every batch that reports removals, the quarantine protocol, descriptor-relative path resolution and recovery (§5.2, §5.3), the no-replace probe, the private directory `.koidrive/` (`staging`, `quarantine`, `parked`, `trash`, `space.json`) left out of scans, and `.koiignore` and `.gitignore` parsing with the engine's matcher (§5.10).
- `core/store`: one SQLite transaction per input, holding R, S and L, the engine state of §2.8, the restore inbox, and the open questions.
- `core/net` and `core/driver`: the WebSocket session, `PrepareUpload` and chunk transfer for `SendCommit`, and back-off on `RETRY_LATER` and on a `QUOTA_EXCEEDED` that names a retry time.
- `core/chunk`: FastCDC 2020 with 1 MiB minimum, 4 MiB average and 16 MiB maximum chunks for files of 4 MiB or more, one chunk below that, none for an empty file, and SHA-256 chunk hashes (spec §10).
- `server/`: the TypeScript Space DO, which must agree with the reference server on spec §6 (§7.1).
