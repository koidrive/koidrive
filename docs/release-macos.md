# Releasing the macOS app

A version tag builds the app from `apps/macos`, signs and notarizes it, and publishes `Koidrive-<version>-arm64.dmg` on the tag's GitHub release, next to the CLI tarballs and in the same `SHA256SUMS`. The [CLI distribution](../.github/workflows/release-cli.yml) workflow does this for the tag and runs the [macOS release](../.github/workflows/release-macos.yml) workflow as one of its jobs. The DMG requires macOS 26 or later on Apple silicon.

Signing needs a Developer ID certificate, an App Store Connect API key and a protected `macos-release` environment. The repository has all three. [Set up release signing](release-macos-signing.md) describes them and how to replace them.

## Release a version

1. Make the release commit on the latest commit of `main`, tag it, and push the tag:

   ```sh
   git fetch origin main && python3 scripts/release-commit.py make v0.1.0-alpha.2 && git push origin v0.1.0-alpha.2
   ```

   The script works in a temporary worktree and leaves your checkout as it is. The release commit stamps the version into `Cargo.toml`, `Cargo.lock` and `server/release.json`, and it stays off `main`. A tag is `v` and three numbers, optionally followed by `-alpha.N` or `-rc.N`. Only repository admins can create `v*` tags. An `-alpha.N` or `-rc.N` tag publishes a normal release, not a prerelease. Unless a higher version is out, that release becomes Latest, which `install.sh` installs by default.
2. Open the CLI distribution run that the tag started. Once the run has built the CLI tarballs, the app and the CLI signing jobs both wait for the `macos-release` environment. Click **Review deployments**, select `macos-release` and **Approve and deploy**. One approval covers both jobs. Approve only a run for a commit and version you meant to release.
3. The `sign-checksums` job signs `SHA256SUMS` with the release key. The `publish` job runs once `release-ref`, `checksums` and `sign-checksums` pass, and `scripts/publish-release.sh` attaches the CLI tarballs, the DMG, `SHA256SUMS` and `SHA256SUMS.sig` to the release in one step. Jobs that don't feed `checksums`, such as `homebrew-install`, can fail after the release is out. When a run fails, check the `publish` job and the release page.

If the app does not build, sign, notarize or verify, the run fails and publishes nothing, not even the CLI tarballs. `publish-release.sh` also refuses when the DMG is missing, when any other file is present (an unsigned DMG included), when `SHA256SUMS` does not list exactly those files with matching hashes, or when `SHA256SUMS.sig` is not a signature of it by the release key in `.github/release-allowed-signers`. A tag that does not point to such a release commit fails in the "commit on main" jobs. There, `scripts/release-commit.py check` stamps the commit's parent again and compares the trees.

## Build a DMG without releasing

Open **Actions**, **macOS release**, **Run workflow**, keep `main` under "Use workflow from", and enter a version such as `0.1.0-alpha.2`, or leave it empty for `0.0.0-dev`. The workflow stamps the app and its bundled CLI with the version you enter. Approve the deployment when the run page asks. The signed DMG is a workflow artifact under **Artifacts** on the run's page, and the run publishes nothing. A run from any other branch fails in the "commit on main" job.

To check a DMG on your Mac:

```sh
mnt=$(mktemp -d)
hdiutil attach Koidrive-0.1.0-arm64.dmg -nobrowse -readonly -mountpoint "$mnt" || exit 1
codesign --verify --deep --strict --verbose=2 "$mnt/Koidrive.app"
xcrun stapler validate "$mnt/Koidrive.app"
spctl --assess --type execute --verbose=2 "$mnt/Koidrive.app"   # expect "source=Notarized Developer ID"
hdiutil detach "$mnt"
```

## What a signed run does

1. A job without the secrets checks that the commit is on `main`, and that a manual run started from `main`.
2. The signing job waits for approval of the `macos-release` environment, which releases the secrets.
3. It builds the Release app with Xcode 27.2 on an `xcode-27` runner, with `koi` in `Contents/Helpers/koi`. `koi` embeds the server bundle, as the CLI release does. `xcode-27` is a GitHub [preview image](https://github.com/actions/runner-images/issues/14404) with macOS 27.
4. It imports the Developer ID certificate into a temporary keychain with a random password, and writes the notary key to a file only the runner user can read.
5. It signs `Contents/Helpers/koi` and then the app with the Developer ID Application identity, the hardened runtime and a secure timestamp, and the app also with [`apps/macos/Koidrive.entitlements`](../apps/macos/Koidrive.entitlements).
6. It notarizes the app with `notarytool` and staples the ticket. `scripts/notarize.sh` submits without waiting, prints the submission id, and asks Apple for the status every minute for up to 2.5 hours. After a network error it tries again after 15 seconds, doubling the wait up to 5 minutes.
7. It packages the stapled app in a DMG, then signs, notarizes and staples the DMG the same way.
8. `scripts/verify-release.sh` checks the DMG and the app with `codesign --verify --deep --strict`, `xcrun stapler validate` and `spctl --assess`, and checks for the hardened runtime, the Developer ID authority and no `get-task-allow`. It also checks that `Contents/Helpers/koi` carries the same kind of signature, and reports the app's `KoidriveVersion`.
9. It uploads `Koidrive-<version>-arm64.dmg` as an artifact and writes its SHA-256 to the run summary.
10. It deletes the keychain and the key files, even when a step failed.

When none of the signing secrets are set, a manual run signs the app ad hoc only, names the DMG `Koidrive-<version>-arm64-unsigned.dmg`, and says "Signing skipped" in a warning and in the summary. Gatekeeper blocks that DMG on other Macs.

## When a run fails

- **"Only some signing secrets are set"** or **"these signing secrets are missing"**. Set all five secrets in the `macos-release` environment ([Set up release signing](release-macos-signing.md#the-five-secrets)). A tag run fails when any one is missing.
- **"Could not import MACOS_DEVELOPER_ID_P12_BASE64"** or **"Expected one valid Developer ID Application identity"**. The `.p12` is not base64-encoded, its password is wrong, or it lacks the private key. Export it again.
- **Apple reports the submission as Invalid or Rejected.** The log shows Apple's notarization log for it.
- **Apple gives no final answer within 2.5 hours**, or a network error ends the run while Apple is still working. The step names the submission id. See the next section.

### Resume an interrupted notarization of the app

Each notarization prints its submission id in the log and as a notice on the run page. `xcrun notarytool info <id>` with the team's API key shows its status. To wait for that submission instead of submitting again, start a new manual run with:

- the same version,
- the failed run's build number as **build-number**. Its "Decide the version" step prints `Version <version>, build <number>.`
- the submission id as **app-submission-id**.

The new run rebuilds the same app, waits for that submission and staples its ticket. If the rebuilt app differs from the submitted one, stapling fails and the step says so. Run again with **app-submission-id** empty to submit the app again. Waiting for an earlier submission works only for manual runs. A tag run submits again when you rerun its failed jobs. The DMG always gets a new submission, because a rebuilt disk image is never identical to an earlier one.

## Versions

The app carries its version in three `Info.plist` keys. For the tag `v0.1.0-alpha.2`:

| Key | Value | Set from |
|---|---|---|
| `CFBundleShortVersionString` | `0.1.0` | The `X.Y.Z` part of the version, because macOS expects three numbers here |
| `CFBundleVersion` | The build number, for example `412` | The run number: of the CLI distribution run for a tag, or of the macOS release run (or the **build-number** input) for a manual run |
| `KoidriveVersion` | `0.1.0-alpha.2` | The full version |

The tag is the release version, and its release commit already carries it, so the release workflows build the tagged tree as it is. A manual app run with a version input runs `python3 scripts/stamp-release-version.py` in the CI checkout. The workspace version on `main` stays `0.0.0-dev`.

`apps/macos/scripts/build-release.sh` takes the full version in `VERSION` and sets the first and third keys. `KoidriveVersion` comes from the `KOIDRIVE_VERSION` build setting in `apps/macos/project.yml`. The script rejects a `koi --version` that differs from `VERSION`. Without `VERSION`, it uses the version `koi` reports. Compare two builds by `KoidriveVersion`, which orders `0.1.0-alpha.2` before `0.1.0-rc.1` and `0.1.0`. Settings shows the full `KoidriveVersion`.

Local builds from `main` report `0.0.0-dev`, including builds with `--release`. They do not offer release updates, and telemetry identifies them as development builds. The CLI, driver, app bindings, and server bundle all read the workspace version. The Homebrew formula uses the tag and hashes from the release's `SHA256SUMS`.

Non-publishing CLI distribution checks use the fixture tag `v0.0.0-rc.0` to test the release-only Homebrew formula path. They do not create a tag or publish a release.

### Check a version without a tag

Use a disposable checkout and a separate target directory:

```sh
python3 scripts/stamp-release-version.py v9.8.7-rc.6
CARGO_TARGET_DIR=/tmp/koidrive-version-check nice -n 10 cargo build --release --locked -p koidrive-cli
test "$(/tmp/koidrive-version-check/release/koi --version)" = 'koi 9.8.7-rc.6'
```

## CI builds an unsigned DMG

The `macos app` job of [CI](../.github/workflows/ci.yml) builds the app, tests its Swift bindings, signs it ad hoc and verifies an unsigned DMG, so a change that breaks packaging fails before a release. It runs on a pull request that changes a file that builds or behaves differently on macOS, in the merge queue, and on a push to `main` that the merge queue did not already cover. It runs on a GitHub-hosted `xcode-27` runner. These runs never see the signing secrets.

## The app needs no entitlements

`apps/macos/Koidrive.entitlements` is empty on purpose. The app ships with Developer ID, not through the Mac App Store, so it is not sandboxed and can sync any folder the person picks in their home folder. The hardened runtime needs an entitlement only to turn off one of its protections ([Hardened Runtime](https://developer.apple.com/documentation/security/hardened-runtime)), and the app needs none. The app links the Rust core statically and uses no JIT, unsigned executable memory, DYLD environment variables or libraries signed by someone else. Open at login uses `SMAppService.mainApp`, which needs no entitlement outside the sandbox.

## Limits

- There is no update feed and no Homebrew cask.
- A run cannot wait for an earlier submission of the disk image. Every run builds a new disk image and submits it.
