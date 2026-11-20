# RFC 0013: Commit tokens

Status: proposed. Applies to spec v0 (format_version 1). Amends spec v0 §7.3 and §7.5, and RFC 0002 §3.

A client tells the server where it stands in the log by a seq alone: `Resume.since_seq` and `Commit.base_seq`. A server that loses commits within one history, as after a storage loss, gives their seqs to later commits. Once other devices have brought the head back up to a device's `since_seq`, the server answers that device's `Resume` with the log after it, and the device's remote tree R ([sync-engine design §2.2](../../docs/design/sync-engine/state-model.md#22-three-trees-keyed-by-node-id)) silently forks from the server: it keeps the lost commits and never receives the ones that took their seqs. A commit built on such a seq passes the base check, and R7 then treats every revision at or below `base_seq` as seen, so a resent `Delete` can trash a node another device moved in at a reused seq.

This RFC names the commit a client stands at by its token as well as its seq. A bare section number such as §2 refers to this RFC; sections of spec v0 are written as spec v0 §N.

## 1. Tokens

The **commit token** at a seq is the pair (`commit_id`, `committed_at_ms`) of the commit the server applied there. Both are in every `CommitRecord`. The token at seq 0 is empty: `commit_id` is the empty string and `committed_at_ms` is 0.

A token names one application of a commit, not only the commit. A commit in flight that the server applied, then lost, then applied again after the resend (RFC 0002 §2, *w* = *a*) gets a new `committed_at_ms`, so its token differs, unless the server's clock gives the same millisecond.

A restore brings back a prefix of the log. If the server holds, at a seq, the application a client saw there, it holds everything before it, so one token is enough.

## 2. Messages

New fields only:

- `Resume`: `since_commit_id = 3`, `since_committed_at_ms = 4`, the token at `since_seq`.
- `Commit`: `base_commit_id = 9`, `base_committed_at_ms = 10`, the token at `base_seq`.
- `CaughtUp`: `head_commit_id = 2`, `head_committed_at_ms = 3`, the token at `head_seq`.
- `TreePage`: `head_commit_id = 4`, `head_committed_at_ms = 5`, the token at `head_seq`.
- `CommitResult`: `committed_at_ms = 4`, the commit's own time, also in a stored result. With the client's `commit_id` it is the token at `seq`. A server returns a result stored before this RFC with its commit's time from the log.

## 3. The check

A token claims nothing when its `commit_id` is empty, its seq is 0, or its seq is below `log_floor_seq`, where the log no longer holds the commit. Otherwise the server reads its commit at that seq, and the token holds when that commit's token is equal. Any other seq the log holds no commit for never holds a token.

- `Resume`: the check runs after the history fence, `since_seq` above the head and `since_seq` below the floor (spec v0 §7.3). A token that does not hold is answered with `ResyncRequired`, reason `AHEAD_OF_SERVER`.
- `Commit`: the check is part of RFC 0002 §3 step 4, after `base_seq` above the head and before `require_head`. A token that does not hold is refused with `AHEAD_OF_SERVER`, and nothing is applied. The stored result for the `commit_id` (step 2) comes first, so a resend of a commit the server holds gets its stored result.

`AHEAD_OF_SERVER` already means that the server lost commits this client applied. The client lists the tree and the trash again (spec v0 §7.5) and sends again what was lost.

## 4. Truncation

`log_floor_seq` is the oldest seq a `Resume` can start after, so the token at the floor stays checkable. A server that truncates its log keeps the floor commit's token, written in the same transaction that moves `log_floor_seq`, and checks a `Resume` or a `Commit` at the floor against it. A `Resume` below the floor gets `LOG_TRUNCATED` before the token check. A `Commit` based below the floor is not checked: its token claims nothing (§3), so a truncated log is not read as a loss. The server does not truncate its log in 0.1.0 (RFC 0006 §9). The simulation in `core/sim` does, and keeps the floor token.

## 5. Clients

The client keeps the token at its cursor and always pairs it with the seq it names. A token paired with another seq makes every `Resume` fail the check, and the client would list the tree forever. The client learns the token:

- from each record it applies, as its cursor moves to that record's seq;
- from the result of its own commit, with that commit's `commit_id`, only when the cursor moves to the result's seq. A result with no `committed_at_ms`, from a server before this RFC, gives no token;
- from the first `TreePage` of a listing, whose `head_seq` the following `Resume` starts from, and from each record replayed onto the listing;
- from a `CaughtUp` whose `head_seq` equals the cursor's seq, when the cursor holds no token, as one stored before this RFC.

A commit names the token at its `base_seq` when it is built, and a resend keeps it.

## 6. Mixed versions

An empty token claims nothing in either direction. A server that implements this RFC lists `commit_tokens` in `Welcome.features`, a required feature (RFC 0034): a client does not sync with a server that lacks it.

- An old client sends no token, and the server skips both checks.
- A new client does not sync with an old server, which lists no `commit_tokens` (RFC 0034 §2).
- A client upgraded from a store without tokens resumes with an empty token until it applies a record or catches up at its cursor's seq.

## 7. Cost

Each `Resume` and `Commit` adds one read of the server's commit at a seq by its primary key, and about 46 bytes: a 36-character ID and a time. `CaughtUp`, `TreePage` and `CommitResult` add the same. When the check fires, the client lists the tree once, as for any `AHEAD_OF_SERVER`.

A re-application that the server gives the same `committed_at_ms` passes the check. That needs a lost response, a loss and a restart within one millisecond, and is accepted.
