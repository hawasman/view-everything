# View Everything

A desktop app for Linux, Windows and macOS that finds files by name instantly and shows what is
using your disk space. It keeps one Index of every file on your Volumes, so searching and sizing
never wait on a scan.

- **Dashboard**: space per Category, a capacity bar per Location, the largest and most recently
  modified files, and when the Index was last synced.
- **Explorer**: a tree and a flat list beside a Treemap, all driven by one Query. Double-click a
  folder anywhere to zoom into it; the zoom is part of the Query, so you can keep refining it.
- **Locations**: local Volumes are indexed by default. Removable disks stay searchable as Offline
  Volumes after you unplug them, mounted network shares are opt-in, and read-only Remotes can be
  indexed over SSH/SFTP, S3 and FTP. Test a Remote's details before you add it, and edit it later
  from Settings.
- **Trash**: Move to Trash and Empty Trash with confirmation.
- **Cleaner**: lists Rust `target/` folders found in the Index (a sibling `Cargo.toml` or a `CACHEDIR.TAG` inside confirms them) with their On-disk Size. Tick them to form a Cleanup Plan, review the exact paths under Advanced, and Clean removes them permanently (not via the Trash) after re-checking each on disk. Remotes, Offline Volumes and read-only mounts are never offered.

The vocabulary used in the app and the code is defined in [CONTEXT.md](CONTEXT.md); design
decisions are recorded in [docs/adr](docs/adr).

## Query language

Every panel in the Explorer follows one Query. Terms separated by spaces must all match; `a|b`
matches either; `!term` excludes; `"quotes"` keep spaces; `*` and `?` match the whole name; a term
containing `/` or `\` matches the full path. Text ignores case unless told otherwise.

| Modifier | Matches |
| --- | --- |
| `ext:mp4;mkv` | files with one of the extensions |
| `type:video` | files of a Category |
| `size:>1gb`, `size:1mb..5mb` | files by Logical Size |
| `dm:2025-01`, `dm:>=yesterday` | files by modification date |
| `regex:^IMG_\d+` | names matching a regular expression |
| `case:Report` | the term, matching letters in exactly that case |
| `wholeword:cat` | the term only as a whole word: `cat` in `a-cat.txt`, not in `category`. Only letters and digits join a word, so `_`, `-`, `.` and spaces separate words. With wildcards (`wholeword:re*t`) the pattern must cover a whole run of words |
| `parent:/home/me/Docs` | Entries directly inside that folder (not deeper); ignores case |
| `depth:2`, `depth:>1`, `depth:1..3` | Entries by levels below the Location root (the root is 0) |
| `child:.git`, `child:*.toml` | folders with a direct child whose name matches |
| `empty:` | folders with no children |
| `dupe:` | files with a Duplicate: identical content on a local Volume, confirmed by hash (hard links to one file are not Duplicates; Remotes and Offline Volumes are never hashed). Candidates are files of at least 1 MB with an equal size, then equal first and last 64 KB, then equal BLAKE3 hash of the whole file. Hashing runs in the background with progress, one file at a time per Volume, and hashes are cached in `hashes.bin` beside the Index until a file's size or modification time changes |

`case:` and `wholeword:` prefix a plain or wildcard term, chain (`case:wholeword:Cat`), and combine
with `!` and `|` (`!wholeword:tmp`, `case:Foo|case:Bar`).

## Install

Download the latest build from the [Releases](../../releases) page:

| Platform | File |
| --- | --- |
| Windows | `view-everything-windows-x86_64.msi`, or the portable `.zip` |
| Linux | `view-everything-x86_64.AppImage` (`chmod +x`, then run it) |
| macOS (Apple Silicon and Intel) | `view-everything-macos-universal.dmg` |

The macOS app is not notarized: the first time, right-click it in Applications and choose
**Open**.

### Verify a download

Every release file (AppImage, `.msi`, portable `.zip`, `.dmg`, `SHA256SUMS` and the AppImage's `.zsync`)
has a `.minisig` signature beside it. Check one with [minisign](https://jedisct1.github.io/minisign/)
and the public key in [packaging/minisign.pub](packaging/minisign.pub):

```sh
minisign -Vm view-everything-x86_64.AppImage -p packaging/minisign.pub
```

`SHA256SUMS` is signed too, so `sha256sum -c SHA256SUMS` after verifying it covers the other files.
The AppImage also carries update information, so AppImageUpdate, Shelly and Gear Lever can update it.

### Updates

The app asks GitHub Releases for a newer version at most once a day. When there is one, a row above
Settings shows the version and a **What's new** link to the release notes. Settings has a switch for
the daily check and a **Check now** button; a failed check only shows a status there. Builds another
tool manages (the AUR package) are compiled without the updater: `cargo build --no-default-features`.

SSH Remotes use your system `ssh` with keys or ssh-agent (no password prompts). S3 and FTP
secrets are kept in the operating system's keyring, never in the settings file.

## Build from source

Needs a stable Rust toolchain.

```sh
cargo build --release
./target/release/view-everything
```

On Linux, GPUI needs these development packages (Debian/Ubuntu names):

```sh
sudo apt-get install pkg-config libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev \
  libxcb1-dev libvulkan-dev libegl1-mesa-dev libfontconfig1-dev libfreetype6-dev
```

Packaging scripts live in [packaging](packaging): `linux/build-appimage.sh`,
`macos/build-dmg.sh`, the WiX source for the Windows MSI, and an AUR `PKGBUILD`.

## Releasing

Release files are signed with a minisign key kept in a GitHub Actions secret; its public half is
`packaging/minisign.pub`, built into the app. One-time setup by the maintainer:

```sh
minisign -G -W -p packaging/minisign.pub -s release.key   # -W: no password, CI cannot type one
gh secret set MINISIGN_SECRET_KEY < release.key            # then keep release.key somewhere safe, not in git
```

Commit the new `packaging/minisign.pub` (the file in the repository is a placeholder until then). The
release workflow fails when the secret is missing.

## Development

The workspace has three crates: `ve-core` (Index, Query, Filter), `ve-scan` (scanning Volumes and
Remotes, watching mounts) and `ve-app` (the GPUI app).

Enable the pre-commit hook once per clone; it runs the same checks as CI (rustfmt, clippy with
warnings denied, and the test suite):

```sh
git config core.hooksPath .githooks
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option. The bundled Instrument Sans and Instrument Serif fonts are under the SIL Open Font
License (see `crates/ve-app/assets/fonts`).
