# Check a published macOS release

Run **Release journeys** in GitHub Actions and enter the published tag, such as `v0.1.0-alpha.4`. The tag must match the release offered by `koidrive/tap`.

Each runtime case gets a separate [GitHub-hosted macOS 26 runner](https://docs.github.com/en/actions/reference/runners/github-hosted-runners). The check refuses local execution and existing Koidrive state. It uses released binaries, preserves quarantine, and does not use a test build or replace signing verification.

| Case | Required result |
| --- | --- |
| Published assets | The Mac CLI and DMG match `SHA256SUMS`; the signed app and DMG pass Gatekeeper assessment and stapled-ticket validation. |
| Clean cask | The public cask installs, and the next command, `koi --version`, reports the requested release. Plain uninstall removes the app and CLI. A second installation followed by zap removes app state without deleting a test document. |
| Formula first | Record whether Homebrew rejects the binary collision or installs the app while skipping its CLI link. The formula's executable and version must remain intact, including after cask removal. Evidence includes the exact warning or error and resulting installation state. |
| Shell installer | The pinned installer runs through `curl \| sh` in a controlling terminal, completes guided setup with the released CLI, and leaves a usable installation. Cleanup stops the test's app and services. |

The manual shell case uses immutable installer commit `2ffddfa8472a87f858698681551cc663e4579f8d`, checked against SHA-256 `5811e418e8b896542327585e19c7f0626ee1d5b64f69ed1c88df2c5f1e1ac522`. It exercises the installer before publication without deploying it. A local Worker and public CLI commands supply test authentication and a shared folder. They do not substitute for the installer or pre-link its folder.

The manual `installer_source` input defaults to `held`, the immutable raw URL. Set it to `live` to download `https://koidrive.com/install.sh` after deployment. Both manual sources must match the same accepted SHA-256 before any installer or bootstrap CLI executes. No custom URL or digest input is accepted.

Automatic release and CLI-distribution checks use the live URL. They resolve the release tag, require the checkout to match its commit, and compare the downloaded script with `install.sh` in that checkout. A ref or digest mismatch fails before installer or bootstrap CLI execution.

Each job uploads sanitized commands, transcripts, assertions, and cleanup results as `release-journeys-*` artifacts. A failed prerequisite, runtime assertion, or cleanup fails the job. Read the formula-conflict artifact first when checking coexistence.

Cleanup checks the native unregister hook and absence of owned launchd jobs. It does not independently inspect the macOS background-task database or assert menu attachment.

Release publication starts this workflow. The CLI distribution workflow also calls it after tap publication because [events produced by `GITHUB_TOKEN` do not start another workflow](https://docs.github.com/en/actions/concepts/security/github_token#when-github_token-triggers-workflow-runs).

CLI distribution runs the Homebrew cases only when both tap entries select the published tag. If the tap retains another release, distribution still checks the published assets and shell installer.

Before the workflow is on the default branch, run the existing **Installer** workflow from the proposed branch with `release_tag` set. That invokes the same reusable workflow without redeploying the site.

For post-deploy acceptance, use **Installer** with `release_tag=v0.1.0-alpha.4` and `installer_source=live`, after confirming that the merged and served script match the accepted digest. The source and downloaded hash are retained in the installer artifact.

Run the non-mutating unit tests locally:

```sh
nice -n 10 python3 scripts/test-release-journeys-unit.py -v
```
