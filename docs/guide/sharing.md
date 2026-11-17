# Share links

A share link lets anyone who has it open one file or one folder of a space in a browser. They need no account and install nothing. A link is read-only. They can browse a shared folder and download files, and they can't change anything. You make links from any of your devices. Only the owner of a space can share from it.

## Make a link

```sh
koi share create ~/Koidrive/Plans/Q3.pptx
```

The command prints the link, and only the link, on stdout, so `link=$(koi share create ...)` works. On stderr it says what the link shares, how long it works and how to end it. Copy the link then, because no other command shows it again. To give someone a link you lost, revoke it and make a new one.

The file or folder must match what is on the server. Before sharing, Koidrive checks the local file identities and contents, including every file in a shared folder. Wait for edits, moves and replacements to sync before making a link. You can't share a file that your [`.koiignore` rules](folders.md) leave out.

A folder's link shows the folder as it is each time someone opens it. Files added to it later show up, and files moved out of it or deleted don't. Moving or renaming the shared folder or file keeps the link working. If the shared item goes to the trash, the link opens nothing until you [restore it](history.md).

### How long it works

A link works for 7 days unless you say otherwise. `--expires` takes minutes (`m`), hours (`h`) or days (`d`), up to 365 days, or `never`:

```sh
koi share create ~/Koidrive/Photos --expires 30d
koi share create ~/Koidrive/Handbook.pdf --expires never
```

### Password

`--password` asks for a password twice without showing it. The password has 8 to 256 characters, and the person who opens the link must type it. After 5 wrong passwords in a row, the link refuses every password for 15 minutes. In a script, give the password on the first line of standard input:

```sh
printf '%s\n' "$PASSWORD" | koi share create ~/Koidrive/Contract.docx --password
```

## List your links

```sh
koi share list
```

```
ID          ENDS       PASSWORD  SHARES
3f9a0c71be  in 6 days            /home/ada/Koidrive/Plans/Q3.pptx
91c2d7e0aa  never      yes       Handbook.pdf in the space Team
```

The list shows the links that still work, newest first, from every space you own. For a space that no folder on this device syncs, the list shows the space's name and the path in it. `--all` adds the links that expired or were revoked. The ID names a link without revealing it, and neither the list nor its `--json` output holds the links themselves.

## Revoke a link

```sh
koi share revoke 3f9a0c71be
```

The link stops working at once, and a download in progress through it stops before its next part. You can also name the link by pasting it instead of its ID. You can't change a link's expiry or password. Revoke the link and make another.

## What the other person sees

The link opens a page that shows the file's name and size with a download button, or the folder's files and subfolders with a download button for each file. Files download as they are, without a preview, and a folder can't be downloaded as one file. On the Workers Free plan, a download through a link can stop once it passes about 490 MiB. A Free plan request can make at most 1,000 subrequests to Cloudflare services, and each part of a file takes two ([Workers limits](https://developers.cloudflare.com/workers/platform/limits/#subrequests)).
