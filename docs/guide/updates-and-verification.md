# Updates and verifying downloads

## Verify a download

Every release file (AppImage, `.msi`, portable `.zip`, `.dmg`, `SHA256SUMS` and the AppImage's `.zsync`) has a `.minisig` signature beside it. Check one with [minisign](https://jedisct1.github.io/minisign/) and the public key in [`packaging/minisign.pub`](../../packaging/minisign.pub):

```sh
minisign -Vm view-everything-x86_64.AppImage -p packaging/minisign.pub
```

`SHA256SUMS` is signed too. After verifying it, `sha256sum -c SHA256SUMS` covers the other files.

## The update check

The app asks GitHub Releases for a newer version at most once a day.
- When there is one, a row above Settings shows the version and a **What's new** link to the release notes.
- Settings has a switch for the daily check and a **Check now** button.
- A failed check only shows a status in Settings.

## Self-update

Where the app can replace itself without admin rights, it downloads the update in the background and checks its minisign signature against the key built into the app ([ADR-0007](../adr/0007-signed-self-updates.md)). The row then offers **Restart to update**. Nothing is replaced unless the signature matches.

| Build | Self-update |
| --- | --- |
| AppImage | ✓ when its folder is writable. AppImageUpdate, Shelly and Gear Lever can also update it, from the update information it carries |
| Windows `.msi` | ✓ one UAC prompt, then `msiexec` upgrades in place |
| Windows portable `.zip` | ✓ the exe is swapped and the old one removed on the next start |
| macOS `.app` | ✓ when its folder is writable |
| AUR package | ✗ built without the updater (`cargo build --no-default-features`); your package manager updates it |

Anywhere else, and when a download fails, the row keeps the **What's new** link.
