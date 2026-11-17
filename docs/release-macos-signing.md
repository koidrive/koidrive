# Set up release signing

A signed macOS release needs five secrets in a protected `macos-release` environment, and a rule that only repository admins create version tags. The repository has all of them. Use this page to check them after a change, to replace a certificate or key, or to set them up again. Releasing itself is in [Releasing the macOS app](release-macos.md).

A Developer ID certificate signs software as you, so the secrets must reach only code that was reviewed and merged to `main`, and only when you approve. Your approval of the environment is the boundary. A deployment rule like `v*` checks only a tag's name, not whether its commit was reviewed ([deployment branches and tags](https://docs.github.com/en/actions/reference/workflows-and-actions/deployments-and-environments#deployment-branches-and-tags)). The workflow's own check that the commit is on `main` runs code from that same commit, so it is a second line of defence. These are repository settings, and the workflow cannot apply them itself.

## Restrict who can create version tags

1. Open the repository's **Settings**. Under "Code, planning, and automation", click **Rules**, then **Rulesets**.
2. Click **New ruleset**, then **New tag ruleset**.
3. Under "Ruleset name", type `Release tags`, and set "Enforcement status" to **Active**.
4. Under "Bypass list", click **Add bypass**, select **Repository admin**, and click **Add selected**.
5. Under "Target tags", click **Add target**, choose **Include by pattern**, type `v*`, and add the pattern.
6. Under "Tag protections", select **Restrict creations**, **Restrict updates** and **Restrict deletions**, and keep **Block force pushes** selected.
7. Click **Create**.

Only repository admins can then create, move or delete `v*` tags. GitHub refuses everyone else, apps with write access included ([available rules](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-rulesets/available-rules-for-rulesets)).

## Create the macos-release environment

1. In **Settings**, click **Environments**, then **New environment**.
2. Type `macos-release` and click **Configure environment**.
3. Select **Required reviewers**, add `gunwooterry`, and leave **Prevent self-review** unselected, because the same person starts and approves release runs. Click **Save protection rules**.
4. Deselect **Allow administrators to bypass configured protection rules** and click **Save protection rules**.
5. Under "Deployment branches and tags", choose **Selected branches and tags**. Add a **Branch** rule for `main` and a **Tag** rule for `v*`.

Each signed run then waits on its run page for approval ([managing environments](https://docs.github.com/en/actions/how-tos/deploy/configure-and-manage-deployments/manage-environments)).

## The five secrets

Add the secrets under **Environment secrets** of `macos-release`, or from a terminal with `gh secret set <NAME> --env macos-release`, which reads the value from standard input. Never add them as repository or organization secrets. A workflow run for a pull request from a branch of this repository receives those, so anyone who can push a branch could read them. A signed build needs all five:

| Secret | Holds |
|---|---|
| `MACOS_DEVELOPER_ID_P12_BASE64` | The Developer ID Application certificate with its private key, exported as a `.p12` file and base64-encoded |
| `MACOS_DEVELOPER_ID_P12_PASSWORD` | The password you chose when you exported the `.p12` |
| `APP_STORE_CONNECT_API_KEY_ID` | The key ID of an App Store Connect team API key |
| `APP_STORE_CONNECT_API_ISSUER_ID` | The issuer ID of the team's App Store Connect API keys |
| `APP_STORE_CONNECT_API_KEY_P8_BASE64` | The API key's private key, the `.p8` file, base64-encoded |

Each run generates its own keychain password, so that is not a secret you set. GitHub never shows a secret again after you save it.

## Create the Developer ID certificate

Only the Account Holder can create a Developer ID certificate ([Developer ID certificates](https://developer.apple.com/help/account/certificates/create-developer-id-certificates/)).

1. On a Mac, open **Keychain Access** and choose **Keychain Access**, **Certificate Assistant**, **Request a Certificate From a Certificate Authority**. Enter your email address, choose **Saved to disk**, and save the `.certSigningRequest` file ([create a certificate signing request](https://developer.apple.com/help/account/certificates/create-a-certificate-signing-request/)).
2. In [Certificates, Identifiers & Profiles](https://developer.apple.com/account/resources/certificates/list), click the add button, choose **Developer ID**, then **Developer ID Application**, upload the request and download the `.cer` file.
3. Double-click the `.cer` file. It appears under **My Certificates** in Keychain Access, with the private key from step 1.
4. In **My Certificates**, select "Developer ID Application: …". The private key must show under the certificate. Choose **File**, **Export Items**, save it as a `.p12`, and set a strong password.
5. Store both secrets:

   ```sh
   base64 -i DeveloperIDApplication.p12 | gh secret set MACOS_DEVELOPER_ID_P12_BASE64 --env macos-release
   gh secret set MACOS_DEVELOPER_ID_P12_PASSWORD --env macos-release   # prompts for the password
   ```

6. Delete the `.p12` and the `.certSigningRequest` files, and delete the certificate and its private key from Keychain Access. The environment secret is the signing key's only copy.

If the secret is lost, create a new certificate. A team can have up to five Developer ID Application certificates. Revoke the old certificate only if its private key leaked, because apps signed with a revoked Developer ID certificate can no longer be installed or opened.

## Create the App Store Connect API key

`notarytool` needs a team API key and does not accept individual API keys ([creating API keys](https://developer.apple.com/documentation/appstoreconnectapi/creating-api-keys-for-app-store-connect-api)). To generate team keys, you need the Admin role in App Store Connect, and the Account Holder must first request access to the App Store Connect API ([role permissions](https://developer.apple.com/help/app-store-connect/reference/account-management/role-permissions/)).

1. In [App Store Connect](https://appstoreconnect.apple.com/), open **Users and Access**, **Integrations**, **App Store Connect API**. If the page asks you to request access, do that first.
2. On **Team Keys**, click **Generate API Key**, give it a name such as "Koidrive notarization", and choose the **Developer** role. If `notarytool` later reports that the key is not authorized, generate a key with the **App Manager** role and revoke the first one.
3. Click **Download API Key**. Apple offers the `.p8` file only once.
4. Copy the key's **Key ID** from its row and the **Issuer ID** shown above the list of keys.
5. Store the three secrets:

   ```sh
   gh secret set APP_STORE_CONNECT_API_KEY_ID --env macos-release      # prompts for the key ID
   gh secret set APP_STORE_CONNECT_API_ISSUER_ID --env macos-release   # prompts for the issuer ID
   base64 -i AuthKey_<key ID>.p8 | gh secret set APP_STORE_CONNECT_API_KEY_P8_BASE64 --env macos-release
   ```

6. Delete the `.p8` file. If the secret is lost or leaks, revoke the key on the same page and generate a new one.

## The release checksum key

Each release publishes `SHA256SUMS.sig`, an `ssh-keygen -Y sign` signature of `SHA256SUMS` in the namespace `koidrive-release`. The `sign-checksums` job signs with the secret `RELEASE_SIGNING_KEY` in the `release-signing` environment, and fails when that key does not match the public key in `.github/release-allowed-signers`. `publish-release.sh` refuses a signature that does not verify against the same file.

Create the `release-signing` environment like `macos-release`, with only a **Tag** rule for `v*` under "Deployment branches and tags". It needs no reviewers, because the job runs after `checksums`, which waits for the approved `macos-release` jobs.

To create or replace the key:

```sh
d=$(mktemp -d)
ssh-keygen -q -t ed25519 -N '' -C koidrive-release -f "$d/key"
gh secret set RELEASE_SIGNING_KEY --env release-signing < "$d/key"
cut -d' ' -f1,2 "$d/key.pub" | sed 's/^/koidrive namespaces="koidrive-release" /'
rm -rf "$d"
```

The last command prints the line for `.github/release-allowed-signers`. The environment secret is the private key's only copy.
