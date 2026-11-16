# Koidrive user guide

Koidrive keeps folders in sync between your computers through a server that you deploy to your own Cloudflare account. File contents are stored as chunks in an R2 bucket in that account. A Durable Object for each space keeps its file tree, its trash and the old versions of its files.

Start with the [Mac app](macos.md) on macOS 26 or later with Apple silicon. For Linux, servers and scripts, [install](install.md) the `koi` command and [set up](setup.md) a server. This guide covers both clients, version 0.1.0.

## Words this guide uses

- **Server.** The Worker, the two Durable Object classes and the R2 bucket in your Cloudflare account. One server serves one owner.
- **Device.** A computer signed in to the server. Each device has its own credential, which you can revoke.
- **Space.** A synced file tree on the server. A space has its own trash and keeps old versions of its files.
- **Linked folder.** A folder on a device that syncs one space. A device links each space to at most one folder.

## Do a task

For setup and everyday sync on a Mac, use the [Mac app guide](macos.md). The following pages cover the CLI:

1. [Install](install.md): install or update the command, or build it from source.
2. [Set up](setup.md): deploy a server and claim it, add devices, create spaces and upgrade the server.
3. [Folders](folders.md): link a folder, and choose what syncs with `.koiignore` and `config.toml`.
4. [Trash, versions and restore](history.md): get back a deleted file or an earlier version.
5. [Share links](sharing.md): let anyone with a link open a file or folder, with an expiry and a password.
6. [Status and problems](status.md): read `koi status`, pause sync, answer a decision, restart a folder, send refused changes again and write a file for a problem report.
7. [Devices](devices.md): list and revoke devices, and get back in after losing every device.
8. [Back up the server](backup.md): copy the R2 bucket outside Cloudflare, and what the copy can restore.
9. [Uninstall](uninstall.md): remove a device, or remove everything.
10. [Agents and scripts](agents.md): read JSON, check health and retry safely from a script.

## Look something up

- [Conflicts](conflicts.md): what happens when two devices change the same file.
- [What syncs](sync-rules.md): what Koidrive syncs, the first link, `.koiignore` rules, the local trash and uploads.
- [Status reference](status-reference.md): folder states, waits and decisions.
- [koi setup](koi-setup.md): the guided setup's questions and options, and scripted setup.
- [Background service and files](service.md): the service on Linux and macOS, running the daemon yourself, `koi update` and the update check, and the files on this machine.
- [JSON output](json-output.md): the envelope, decisions, retries and exit statuses.
- [Limits](limits.md): what the Workers Free plan allows, and Koidrive's own limits.
- [Usage reports](telemetry.md): what usage reports hold, and how to turn them off.
