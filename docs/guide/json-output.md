# JSON output

This page describes what `koi` prints with `--json` and the exit statuses it returns. [Agents and scripts](agents.md) shows how to use them.

## --json selects schema 1

`--json` works on every command and in any position. It selects schema 1, the only schema. `--json=1` names it explicitly, and any other version is a usage error. Plain `--json` always means schema 1.

With `--json`, every command except `koi _daemon` prints exactly one JSON object on stdout, also when it fails, when it shows help and when it shows the version. The daemon prints one object per line, as [The daemon prints events](#the-daemon-prints-events) describes. Logs and progress go to stderr. A command with `--json` never opens a browser and prints no pending-decision line. It reads stdin only when an explicit input flag requests it, such as `koi setup --pairing-code-stdin` or `koi share create --password`. Only `koi share create --password` asks for input on a terminal.

Without `--json`, a command whose result is one value prints only that value on stdout, and what it means on stderr: `koi space create` prints the new space's ID, `koi history restore` the restore's ID, and `koi device invite` the pairing code.

## The envelope

| Field | Meaning |
|---|---|
| `schema_version` | Always 1 |
| `command` | The subcommand, such as `folder link`. Empty for `koi` alone |
| `outcome` | `ok`, `accepted`, `unchanged` or `error` |
| `data` | The command's result. Its shape depends on the command. `null` when there is none |
| `error` | `null` on success. Otherwise `code`, `message`, `retryable` and, for some refusals, `reason` |
| `decisions` | The sync decisions waiting on this device, described below |
| `next_commands` | Commands to run next, each as an `argv` array. Present only when there is a real next step |

`accepted` means the command recorded work that happens later. Answering a decision and asking for a restore return it. Check `koi status` or `koi history requests` to see the work finish.

`unchanged` means the state the command asks for was already there, so the command changed nothing. It exits 0, and `data` describes the state as it is.

A failed command can still carry `data`. For example, `koi space usage` returns the spaces that answered when the server refuses another one.

An app update can replace files before a later step fails. `data.app.committed` records each replacement's path, full version and `installed` or `replaced` change. With `data.app.state` set to `failed`, these are historical records, not a verified current app. In an app-only partial error, `data.result` stays `updated` and `data.to` names the last committed app version. The service result is independent.

Each command's envelope has a JSON Schema (draft 2020-12) in [docs/json-schema/v1](../json-schema/v1), named after the command: `koi-folder-link.json` for `koi folder link`, and `koi.json` for `koi` alone. `koi-help.json` covers `koi help` with any command after it. Schema 1 can gain optional fields, so a validator should allow fields it doesn't know.

## koi help --json lists every command

`koi help --json`, or `--help --json` after any command, lists that command and every command under it in `data.commands`. Each entry has the command's `about`, its `usage` line, `aliases`, `arguments`, `options`, `groups` and `subcommands`. Each argument and option lists what it `requires` and `conflicts` with, and an option says whether its value is optional and must follow an equals sign, as `--json=1` does. A group names options of which one must be given, or at most one may be.

The entry also has `auth`, `output` and `schema`. `auth` is `none`, `device` when the command needs this device's credential, or `cloudflare` when it needs a Cloudflare login that only `--interactive` starts. `output` is `envelope`, or `lines` for `koi _daemon`. `schema` is the file of the command's schema in docs/json-schema/v1.

## Decisions

`koi status` and `koi resolve` list every pending decision in `decisions`. Every other command lists the decisions the running daemon holds. Each decision has every item it holds and the full command for each answer:

```json
{
  "id": "019b...",
  "space_id": "019a...",
  "kind": "mass_delete",
  "asked_at_ms": 1762900000000,
  "requires_choice": true,
  "counts": { "deletions": 24, "affected_files": 1240, "space_files": 2000, "exceeded": ["deletions"] },
  "items": [{ "node_id": "019c...", "path": "archive", "files": 1240 }],
  "answers": [
    { "token": "delete", "consequence": "Delete these files on the server and other devices", "argv": ["koi", "resolve", "019b...", "delete"] },
    { "token": "restore", "consequence": "Download these files to this device again", "argv": ["koi", "resolve", "019b...", "restore"] }
  ]
}
```

`kind` is `mass_delete`, `remote_mass_delete`, `ambiguous_disappearance`, `recovery` or `conflict`. A `recovery` decision also has a `group`. A `conflict` decision's `items` are its two files, and its answers are `keep-node:<id>` for each file's node ID and `keep-both`. A `remote_mass_delete` notice has `requires_choice: false`. It only takes `dismiss` and holds nothing back. [Decisions](status-reference.md#decisions) explains each kind.

## Retries never make a change twice

These commands set a state, so running one again when the state already holds returns `unchanged` with exit status 0:

- `koi folder link <folder> --space <space>` when the folder is already linked to that space. Linking it to another space fails and changes nothing. With a space name rather than an ID, the command reads the server's space list, so it fails while the server can't be reached.
- `koi folder unlink <folder>` when the folder isn't linked. `data` then has only `path`.
- `koi device revoke <id>` when the device is already revoked.
- `koi space create <name>` when a space has that name. It prints that space's ID, as for a new space.
- `koi resolve <id> <answer>` with the answer the decision already took. Another answer, or a decision that was replaced, fails with `stale_decision` (exit 7).

These commands make a new change each time they run: `koi history restore`, `koi device invite`, `koi folder restart` and `koi folder retry`. Give them `--request-id <id>` on the first attempt. The ID is 1 to 128 ASCII letters, digits, `-` and `_`, and you pick it. Running the command again with the same ID and arguments returns the first outcome, with the same `data` and the same stdout, and changes nothing. This holds after the daemon or the machine restarts.

- The same ID with other arguments fails with `stale_decision` (exit 7) and no next command. Repeat the first call as it was, or pick a new ID. An ID for `koi device invite` also belongs to the device and server that ran it.
- A pairing code expires after about 10 minutes. After that, `koi device invite` with its ID fails with `stale_decision`. Run it with a new ID.
- A command whose change is known to have failed records nothing, so running it again with its ID tries again.
- If the first run stopped before it recorded its outcome, still runs, or couldn't tell whether its change happened (for example, the server didn't answer), the command fails with `interrupted` (exit 130). Wait until the first run has ended, then read the state with `koi status --json` or `koi history requests`. If its change didn't happen, make it with a new ID.

`koi share create` takes no request ID, because its record would keep the link. Each run makes a new link. If a run fails without an answer, `koi share list --json` shows whether it made one, and `koi share revoke` ends a link you don't need.

The records are in `requests/` in the profile's state directory (`~/.local/state/koidrive` by default), readable only by you. A record of `koi device invite` holds the pairing code. Pairing codes never appear in the logs.

## Exit status

| Status | Meaning | What to do |
|---|---|---|
| 0 | The command succeeded, found nothing to change, or recorded work for later | Read `outcome`. `accepted` isn't done yet |
| 1 | Another failure | Read `error` and `next_commands` |
| 2 | Invalid arguments, or input the command needs is missing | Correct the call. Running it again unchanged fails again |
| 3 | `status --check` found a decision that needs a choice | Read the answers in `decisions` |
| 4 | A credential is missing or refused, or a person must log in | Run the command in `next_commands`, or ask a person to |
| 5 | A folder is halted or stopped | Fix the cause the folder's `problem` names |
| 6 | The server is offline, unavailable or limits the rate | Try again later. `retryable` is true |
| 7 | The decision answered no longer exists, or a request ID was used with other arguments or its pairing code expired | Read the current state, then decide again |
| 8 | `koi join` or `koi server claim` got a claim or pairing code that is wrong or has expired | Ask the person for the code again |
| 130 | Ctrl-C stopped the command, or an earlier run with the same request ID has no recorded outcome | Read the state before you run a change again |

The `error.code` values match these: `failed`, `usage`, `decision_required`, `auth_required`, `halted`, `unavailable`, `stale_decision`, `wrong_code` and `interrupted`. A `koi history` or `koi share` refusal also names its `error.reason`, such as `not_in_linked_folder` or `no_such_version`; the schema lists every reason.

## status --check reports the most urgent condition

`koi status` exits 0 whenever it can read the status. `koi status --check` exits with the status of the most urgent condition:

1. 4 when this device isn't set up.
2. 5 when a folder is halted, or stopped because the daemon isn't running. `service` in the same output says whether the background service runs and why not.
3. 3 when a decision needs a choice. A notice that only takes `dismiss` doesn't count.
4. 6 when a folder is offline, or puts a call off until later. The folder's `wait` says why: `quota` when the server refused for a used-up limit or full storage, `retry` when the call kept failing for another reason. `until_ms` is when it's sent again.

Connecting and catching up aren't failures, so they exit 0. Neither is a pause. While sync is paused, `data.paused` is true (it's left out otherwise), the folders the pause stopped add no `stopped` condition, and `next_commands` names `koi resume`. A halted folder still adds a `halted` condition while paused. `data.conditions` lists every condition found, with or without `--check`.

`data.update` says whether a newer release is out: `state` is `available`, `up_to_date`, `unknown` (no check has had an answer yet) or `off`, and `version` is the version this device runs, the daemon's or this `koi`'s when no daemon runs. With `available` it adds `latest`, `url` and `command`, which is `koi update`. `checked_at` is when GitHub last answered, in UTC. `--check` ignores it.

## The daemon prints events

`koi _daemon --json` prints one JSON object per line on stdout, each with `schema_version` and `event`: `started` with the version, the socket and the number of folders, `folder_state` with the folder, its space and its new state whenever a folder's state changes, `stopping` with the signal, and `stopped`. Its logs on stderr are JSON lines too. If it can't start, it prints one error envelope on a single line.

## Questions need a terminal

No command asks a question unless both stdin and stderr are terminals. Without them, commands that would ask stop instead and say which flag answers the question.

- `koi setup` runs the guided setup only on a terminal, without `--json` and without the `CI` environment variable. Otherwise it lists the setup commands and exits 0. With `--json` they are in `data.choices` and `next_commands`. `--folder` and `--space`, with `--pairing-code-stdin` on a new device, run a [scripted setup](koi-setup.md#scripted-setup) that asks nothing.
- `--interactive` asks for the guided setup explicitly. It fails with exit status 2 when no terminal is attached, and can't be used with `--json`.
- `koi server deploy`, `add-device`, `upgrade` and `delete` log in with Cloudflare in a browser, so they run only with `--interactive` and a terminal on stdin and stderr. Otherwise, with or without `--json`, they stop with `auth_required` (exit 4) and name the same command with `--interactive` for a person to run.
- `--no-browser` reads the login address from the terminal, so it needs `--interactive` too.
