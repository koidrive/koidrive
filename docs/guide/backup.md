# Back up the server

Koidrive has no backup of its own. If you delete the R2 bucket or lose the Cloudflare account and have no copy elsewhere, nobody can get back what only the server held: the trash, the old versions, and every file that no device still has. This page copies the bucket to storage outside Cloudflare with [rclone](https://rclone.org/). The copy can put back objects the bucket lost while the server still runs, but [not a lost account](#after-you-lose-the-account).

## Copy the bucket

The steps follow Cloudflare's [rclone guide](https://developers.cloudflare.com/r2/examples/rclone/) and [R2 authentication](https://developers.cloudflare.com/r2/api/tokens/) pages.

1. Find the bucket's name on the **R2 object storage** page of the Cloudflare dashboard. `koi setup` names it `koidrive-` followed by 16 hex digits. Wrangler uses the name in `wrangler.jsonc`, `koidrive` by default. The commands below write it as `<bucket>`.
2. On the same page, select **Manage** next to **API Tokens**. Create a token with the **Object Read only** permission, scoped to `<bucket>`. Note its **Access Key ID** and **Secret Access Key**, and your account ID.
3. Install rclone 1.59 or later. Earlier versions can fail against R2.
4. Add a remote for R2 to the rclone configuration file. `rclone config file` prints where it is.

   ```ini
   [r2]
   type = s3
   provider = Cloudflare
   access_key_id = <Access Key ID>
   secret_access_key = <Secret Access Key>
   endpoint = https://<account ID>.r2.cloudflarestorage.com
   acl = private
   no_check_bucket = true
   ```

   A token with an Object permission needs `no_check_bucket = true`. If the bucket is in a jurisdiction, the endpoint is `https://<account ID>.eu.r2.cloudflarestorage.com` for `eu`, `.us.` for `us` and `.fedramp.` for FedRAMP.
5. Copy the export first and the chunks second, so that the chunks the export names are already in the bucket. Any rclone remote can replace `/backup/koidrive`.

   ```sh
   rclone copy r2:<bucket> /backup/koidrive --include "/spaces/*/meta/**"
   rclone copy r2:<bucket> /backup/koidrive --include "/spaces/*/chunks/**"
   ```

6. Run both commands again on a schedule, for example from `cron`. Each run transfers only new objects.

Use `rclone copy`, never `rclone sync`, which would empty the copy after a bucket was emptied by mistake.

Koidrive doesn't encrypt chunks. Anyone who can read the copy can read your files, so keep it as private as the bucket.

## What the copy holds

Koidrive writes only under `spaces/<space_id>/`. `chunks/` holds the file contents, named by their SHA-256. `meta/` holds an export of the space's tree, trash and old versions, in the format [RFC 0004](../../spec/rfc/0004-r2-export-format.md) describes. The server exports once 1,000 commits wait, or 5 minutes after the first commit it hasn't exported, so a copy can miss the last few minutes of changes.

## Put lost objects back

Use this when the server still runs but objects in its bucket are gone. Files whose chunks are gone fail to download, and sync doesn't upload them again.

1. Create a token as in step 2 above, with the **Object Read & Write** permission instead, and put it in the `r2` remote.
2. Copy everything back:

   ```sh
   rclone copy /backup/koidrive r2:<bucket>
   ```

The objects return under the same names, and the server serves each chunk again. If you deleted the bucket itself, first create a bucket with the same name, in the same jurisdiction.

## After you lose the account

A copy can't bring back a server. A new server never reads `meta/`, so copying the backup into its bucket restores nothing. The copy still holds each space's tree, trash and old versions from the last export it caught. RFC 0004 describes how to read it, but Koidrive has no tool that does. The live files in your linked folders stay on each device.
