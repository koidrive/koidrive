# Agents and scripts

Every `koi` command except the ones that log in with Cloudflare can run without a person at the terminal. Add `--json`, read one JSON object from stdout, then branch on the exit status. `koi _daemon --json` is the exception. It prints one object per line until it stops. Start with `koi status --json`.

## Ask for JSON

`--json` works on every command and in any position, as in `koi --json status` or `koi folder link ~/Work --space Work --json`. The command prints exactly one JSON object on stdout, even when it fails, and writes logs to stderr. `koi _daemon` prints a [stream of events](json-output.md#the-daemon-prints-events) instead. With `--json`, a command never opens a browser, and only `koi share create --password` asks for input on a terminal.

```json
{
  "schema_version": 1,
  "command": "history restore",
  "outcome": "accepted",
  "data": { "request_id": "019b...", "folder": "/home/alice/Koidrive", "path": "notes/old.txt" },
  "error": null,
  "decisions": [],
  "next_commands": [{ "argv": ["koi", "history", "requests", "/home/alice/Koidrive"] }]
}
```

Read `outcome` first: `ok`, `accepted` for work that finishes later, `unchanged`, or `error`. When `next_commands` is present, it holds the commands to run next. [JSON output](json-output.md) describes every field, the schemas and the exit statuses.

Without `--json`, a command whose result is one value prints only that value on stdout, so you can capture it in a shell variable: `id=$(koi space create Notes)`.

To list every command with its arguments, options and schema, run `koi help --json`.

## Check health with koi status --check

```sh
koi status --check --json
```

The exit status names the most urgent condition: 4 when this device isn't set up, 5 when a folder is halted or stopped, 3 when a decision needs a choice, 6 when a folder is offline or waits, and 0 otherwise. The command also exits 0 while a folder connects or catches up, or while sync is paused. `data.conditions` lists every condition found. [status --check](json-output.md#status---check-reports-the-most-urgent-condition) has the details.

## Set up a device without a person

`koi join` and `koi server claim` never ask questions, so an agent can add a device with a pairing code or claim a server with a claim code. To pair and link a folder in one command, give the pairing code on stdin:

```sh
printf '%s\n' "$KOI_PAIRING_CODE" | koi setup --pairing-code-stdin \
  --folder "$HOME/Koidrive" --space "$KOI_SPACE" --telemetry off --json
```

[Scripted setup](koi-setup.md#scripted-setup) lists the rules. The commands that log in with Cloudflare need a person at a terminal. Without `--interactive`, they stop with `auth_required` (exit 4) and name the command for a person to run.

## Run a change again safely

A command can time out before you see its result. Commands that set a state, such as `koi folder link`, `koi folder unlink`, `koi space create`, `koi device revoke` and `koi resolve`, return `unchanged` with exit status 0 when you run them again.

Commands that make a new change each time take `--request-id`. Pick an ID on the first attempt and repeat it on every retry:

```sh
koi history restore ~/Koidrive/notes/old.txt --request-id restore-notes-1 --json
```

The same ID with the same arguments returns the first outcome and changes nothing. `koi history restore`, `koi device invite`, `koi folder restart` and `koi folder retry` take it. [Retries](json-output.md#retries-never-make-a-change-twice) covers the edge cases.

## Leave deletes to a person

`koi status` and `koi resolve` list each pending decision in `decisions`, with the full `argv` for each answer. Don't pick an answer that deletes files yourself. Show the decision to the person and run the answer they choose.
