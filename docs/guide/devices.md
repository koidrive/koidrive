# Devices

## List devices

```sh
koi device list
```

This lists every device signed in to your server, revoked ones included, with its ID, name, platform, when it was last seen and its state. `--json` prints the same.

## Revoke a device

```sh
koi device revoke <id>
```

Take the full ID from `koi device list`. The device stops syncing at once. The files already on it stay there, so revoking a stolen laptop stops new changes from reaching it but doesn't erase it. A device can't revoke itself, so revoke it from another device.

A revoked device can't sync any more. To use that machine again, remove Koidrive from it as [Uninstall](uninstall.md#remove-koidrive-from-one-device) describes, then set it up again.

## Sign this device out

```sh
koi device leave
```

This revokes the device on the server and unlinks every folder. The files stay where they are. If the server refuses because this is your account's last active device, Koidrive keeps the local pairing and folder links.

If revocation is interrupted or the server doesn't answer, its outcome may be unknown. While the outcome is unknown, sync stays stopped and this device's account stays blocked. Run `koi device leave` again to resolve the saved sign-out.

Add `--last-device` to confirm signing out anyway. If the server is unreachable, this removes only the local pairing and links. The server may still accept the device's credential, so revoke it from another device when the server is reachable. If you sign out of every device, use [owner recovery](#if-you-lose-every-device) to sign in again.

## If you lose every device

Your files stay on the server. Without the [Cloudflare login](koi-setup.md#the-cloudflare-login-and-what-it-may-do), set a recovery key on the Worker and claim the server with it, as [If you lose every device](../deploy.md#if-you-lose-every-device) in the deploy guide describes.

With the Cloudflare login, run this on a terminal, with a `koi` that [includes the server](install.md#build-from-source):

```sh
koi server add-device --interactive
```

You log in with the Cloudflare account that holds the server. That proves you own the server, so Koidrive needs no other device. It sets a one-time recovery key on the Worker, adds this machine as a new device of your account, and deletes the key again.

Then revoke the lost devices with `koi device revoke`, since recovery doesn't sign them out.
