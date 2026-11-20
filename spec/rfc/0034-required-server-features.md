# RFC 0034: Required server features

Status: proposed. Applies to spec v0 (format_version 1). Amends RFC 0022 §2, §3 rule 1 and §4, and RFC 0013 §6. No message changes.

A client can depend on server behaviour that an RFC adds without moving the protocol range (spec v0 §9.1). RFC 0013 is the first case: a server without it answers a commit with no `committed_at_ms`, and a client that recovers from a server loss (RFC 0002 §3) then records its own commits at time 0. The protocol version does not tell the client which server it talks to, so it needs another way to refuse that server and tell the person what to fix.

This RFC lets the server announce such a behaviour as a required feature in `Welcome.features` (RFC 0022), and makes the client refuse to sync with a server that does not list it. A bare section number such as §2 refers to this RFC.

## 1. Required features

A **required feature** is a feature (RFC 0022 §2) that a client needs before it syncs with a server.

1. Every server release from the one that adds a required feature implements it and always has it configured. `KOIDRIVE_FEATURES` cannot turn it off: `-name` for a required feature is ignored, as an unknown name is.
2. RFC 0022 §3 rule 2 applies as to any feature: `Welcome.features` lists a required feature when the sending Space DO's code implements it, and `ServerInfo.features` lists it once the Account DO's code does.
3. A required feature is never off, so RFC 0022 §3 rules 4, 5, 6 and 8 never apply to it.

## 2. Client rules

1. After the protocol check of a `Welcome`, a client checks that `Welcome.features` lists every required feature it implements.
2. If one is missing, the client sends nothing more on the connection, closes it, and does not treat it as connected. It tells the person that the server lacks the features it names, shows `Welcome.server_version`, and says to run `koi server upgrade`, or, if it just ran, that the server finishes updating within 5 minutes (RFC 0022 §3 rule 2).
3. The client then connects again with its usual backoff. It never stops for good on a missing required feature: a Space DO can run the previous release for up to 300 seconds after a deploy (RFC 0022 §3 rule 2), and an updated server must get the client syncing again without a restart.
4. The client decides from `Welcome` alone and does not read `ServerInfo` for this check, so RFC 0022 §4 rule 2 does not apply.

## 3. Registered required features

| Name | RFC | What the client needs |
|---|---|---|
| `commit_tokens` | RFC 0013 | The token fields of RFC 0013 §2 in `CaughtUp`, `TreePage` and `CommitResult`, and the token checks of RFC 0013 §3 |

To add a required feature, an RFC registers its name here and in RFC 0022 §2, and the server and the client each add the name to their list of required features.

## 4. Amendments

- RFC 0022 §2 registers `commit_tokens` for RFC 0013.
- RFC 0022 §3 rule 1: a required feature is always configured, and `KOIDRIVE_FEATURES` cannot turn it off (§1).
- RFC 0022 §4: a client refuses to sync with a server whose `Welcome` lacks a required feature (§2).
- RFC 0013 §6: a server that implements RFC 0013 lists `commit_tokens`.

## 5. Compatibility

- A server without RFC 0013 lists no `commit_tokens`. A client with this RFC asks the person to update it and syncs once it lists the feature.
- A server that implements RFC 0013 but not this RFC lists no `commit_tokens` either, and gets the same answer.
- A client without this RFC ignores the name (RFC 0022 §4 rule 5).

## 6. Tests

- Server: `Welcome` and `ServerInfo` list `commit_tokens` by default and with `KOIDRIVE_FEATURES=-commit_tokens`.
- Client: a `Welcome` without `commit_tokens` gives no connected event and sends nothing more; the disconnect names the server's version, the missing feature and `koi server upgrade`; the client connects once a `Welcome` lists the feature. The folder status shows that text.
