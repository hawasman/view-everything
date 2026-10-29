# View Everything

A desktop app for Linux, Windows and macOS that finds files by name instantly and shows what is
using your disk space. It keeps one Index of every file on your Volumes, so searching and sizing
never wait on a scan.

- **Dashboard**: space per Category, a capacity bar per Location, the largest and most recently
  modified files, and when the Index was last synced.
- **Explorer**: a tree and a flat list beside a Treemap, all driven by one Query. Double-click a
  folder anywhere to zoom into it; the zoom is part of the Query, so you can keep refining it.
- **Compare with**: the Index saves a Snapshot (the Folder Size of every folder, by path) on its first save each day once every Location has synced, in a `snapshots/` folder beside `index.bin`; the last 30 days are kept, then the first Snapshot of each month. **Compare with** in the Explorer tree picks one of those dates and adds a sortable **Change** column (the growth or shrinkage of each folder; a folder that did not exist then reads as new, and one that is gone is listed under its parent as removed). The comparison runs in the background.
- **Export**: Export CSV writes what the flat list shows (every Entry matching the Query, so a zoomed folder is included) with the columns path, name, Logical Size, On-disk Size, modified (UTC) and Category; a name a spreadsheet could read as a formula (starting with `=`, `+`, `-`, `@`, tab or CR) gets a leading `'`. Export PNG on the Treemap saves the tiles as drawn, at 2x, in the current theme's colours (without the text labels). Both ask where to save, or use the Downloads folder where there is no save dialog.
- **Locations**: local Volumes are indexed by default. Removable disks stay searchable as Offline
  Volumes after you unplug them, mounted network shares are opt-in, and read-only Remotes can be
  indexed over SSH/SFTP, S3, FTP, WebDAV and Azure Blob. Test a Remote's details before you add it, and edit it later
  from Settings.
- **Live updates**: on macOS the Index follows changes with FSEvents while the app runs (a file created, renamed, resized or deleted shows within seconds), and at launch replays only the changes since the event id saved with the Index, so catching up is cheap; if macOS refuses or has dropped that history, the Location is re-walked. On Linux, following a whole disk needs root, so it is opt-in: Settings → Live updates starts a small helper (`ve-fanotify`) with `pkexec` while the app runs. It stops when the app does or when you switch it off, only reports which paths changed, and never writes anything. It is installed with the package (AUR) in `/usr/libexec/view-everything/` and does not work from inside the AppImage, which root cannot read. If you deny the password prompt the app says so and keeps re-walking periodically, as it does without the helper. After a reboot, Linux catches up by re-walking.
- **Background**: Settings → Background keeps the app in the system tray when its window closes (so the Index stays fresh, Live updates and Tasks keep running; tray menu: Open, Sync now, running Tasks, Quit) and starts it at login (XDG autostart on Linux, a highest-privileges scheduled task on Windows, a LaunchAgent on macOS). On Linux the tray needs a StatusNotifierItem host (GNOME needs the AppIndicator extension, `gnome-shell-extension-appindicator` on Arch); without one the options are disabled and closing the window quits. A login start does not start the Live updates helper, to spare the password prompt; start it from the tray menu or the Dashboard.
- **Trash**: Move to Trash and Empty Trash with confirmation.
- **Cleaner**: lists what can be regenerated or discarded, grouped as Temporary Files, Caches and Build Artefacts, with each one's On-disk Size. Built-in Cleanup Rules cover the user temp and cache folders (on Windows the Internet cache and shader cache), Rust `target/`, Next.js `.next`, `node_modules` and other tool output; you can add [your own](#cleanup-rules). Build artefacts count only when a project marker is found (a sibling `Cargo.toml`, `package.json`, …). Temporary files are pre-ticked when nothing in them changed for 7 days and build artefacts when nothing in the project changed for 30 days (both limits are in Settings); caches and `node_modules` are never pre-ticked. The Cleanup Plan summary and its buttons (Advanced, Clean, Clean as admin) sit at the top, with Advanced opening right below them. A tab per group (Temporary Files, Caches, Build Artefacts, Admin only, each with its count) shows that group's Cleanables 25 at a time (25/50/100), in a list with a scrollbar and previous/next controls; ticks stay when you change page or tab, and the Cleanup Plan covers every ticked Cleanable. A Category breakdown (the Explorer's panel) shows how much of the reclaimable space each Category takes, for all Cleanables or for the ticked Cleanup Plan; it is summed from what each Cleanable holds in the Index, read in the background with the look. Tick them to form a Cleanup Plan, review the exact paths and commands under Advanced, and Clean removes them permanently (not via the Trash) after re-checking each on disk (still the same kind of folder, marker present, no link on the way to it, nothing in it changed within the age limit if it was ticked for being old). Some rules remove by running a tool's own command (such as `npm cache clean --force`): it runs without a shell, its size is only an Estimated one, and its output and exit status are shown after Clean. Remotes, Offline Volumes and read-only mounts are never offered. Cleanables you do not own, or that hold something you do not own (such as `/var/cache/pacman`, or what is under `C:\Windows` on Windows), are listed separately under Admin only with the exact command that would remove them; the normal Clean never touches them. On Linux, Clean as admin shows the command and runs it with `pkexec` (one password prompt), then reports per item whether it is gone; on macOS and Windows the app does not elevate, so run the shown command yourself. Command rules are never run with admin rights. Each Cleanable has Show in Explorer, which sets the Query to its folder, and the Dashboard has a Cleaner card with the total the Cleaner could reclaim (it updates after Clean) that opens the Cleaner.
- **Duplicates**: the Explorer's third tab groups files with identical content (see `dupe:` below), largest wasted space first. Pick the Duplicate to keep in each group; **Trash others** moves the rest to the Trash after the usual confirmation, and the Index and groups update. The list is virtualised, so any number of groups or copies stays light. Hashing runs in the background with progress in the status bar.

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
| `dc:2025-01`, `dc:<2020` | files by creation date, written like `dm:`. Never matches a file whose creation time is unknown: S3 Remotes, FTP servers without the MLSD `create` fact, SFTP, filesystems without a birth time |
| `regex:^IMG_\d+` | names matching a regular expression |
| `case:Report` | the term, matching letters in exactly that case |
| `wholeword:cat` | the term only as a whole word: `cat` in `a-cat.txt`, not in `category`. Only letters and digits join a word, so `_`, `-`, `.` and spaces separate words. With wildcards (`wholeword:re*t`) the pattern must cover a whole run of words |
| `parent:/home/me/Docs` | Entries directly inside that folder (not deeper); ignores case |
| `depth:2`, `depth:>1`, `depth:1..3` | Entries by levels below the Location root (the root is 0) |
| `child:.git`, `child:*.toml` | folders with a direct child whose name matches |
| `empty:` | folders with no children |
| `dupe:` | files with a Duplicate: identical content on a local Volume, confirmed by hash (hard links to one file are not Duplicates, and only one name of a hard-linked file is listed; Remotes and Offline Volumes are never hashed). Candidates are files of at least 1 MB with an equal size, then equal first and last 64 KB, then equal BLAKE3 hash of the whole file. Hashing runs in the background with progress, one file at a time per Volume, opens only regular files (never a symlink, FIFO, socket or device, even if one replaced a file since it was indexed), reads at most the size the Index recorded, drops a file that changed while hashed, and is redone for Index changes at most every 5 minutes, and hashes are cached in `hashes.bin` beside the Index until a file's size or modification time changes |

`case:` and `wholeword:` prefix a plain or wildcard term, chain (`case:wholeword:Cat`), and combine
with `!` and `|` (`!wholeword:tmp`, `case:Foo|case:Bar`).

## Command line

`ve` reads the saved Index (it never writes it, so it works while the app runs; open the app once to
build the Index). It ships beside the app: in `usr/bin` of the AppImage (`./view-everything-x86_64.AppImage --appimage-extract`
to get it out), next to `view-everything.exe` on Windows, and in `View Everything.app/Contents/MacOS/` on macOS.

```
ve find [--csv|--json] [--path DIR] <QUERY>...   matching paths, one per line
ve top  [--csv|--json] [--path DIR] [--files] [-n N] [QUERY]...   largest folders (or files) by On-disk Size (Filtered Size under a Query); `du` is the same
ve export [--json] [--path DIR] [QUERY]...         CSV of a Query's results (everything without one)
```

The Query is the same language as the app's; quote it for your shell or put `--` before words that start with `-`.
`top` prints `<bytes>\t<path>`. `--index FILE` reads another saved Index. The Index's Last Synced goes to stderr so
pipes stay clean. Exit code 0: something was printed; 1: no match; 2: a bad argument, Query or unreadable Index.
`dupe:` finds nothing from the CLI (it does not read `hashes.bin`).

## Cleanup rules

The Cleaner's rules are written in TOML; the built-in ones (`crates/ve-app/src/cleaner/builtin.toml`) use the same format as your own, which go in `cleanup.toml` next to `settings.toml` (`~/.config/view-everything/` on Linux, `~/Library/Application Support/view-everything/` on macOS, `%APPDATA%\view-everything\` on Windows). The file is read again whenever it changes. A mistake shows in the Cleaner with the file, line and reason; the built-in rules keep working (a syntax error skips the whole file, a bad rule only itself).

```toml
[[rule]]
name = "Python virtualenvs"      # shown in the Cleaner
group = "build"                  # temp, cache or build
query = 'regex:^\.venv$'         # a Query: what the rule matches
markers = ["../pyproject.toml", "../requirements.txt"]   # one must be a plain file
tick = false                     # never pre-ticked

[[rule]]
name = "cargo clean"
group = "build"
query = 'child:Cargo.toml'       # folders holding a Cargo.toml
markers = ["Cargo.toml"]
command = ["cargo", "clean"]     # run in the matched folder, without a shell
estimate = "target"              # what to size for the Estimated figure
```

| Key | |
| --- | --- |
| `name`, `group` | Required. `group` is `temp` (Temporary Files), `cache` (Caches) or `build` (Build Artefacts). |
| `query` | Required. A [Query](#query-language) over the Index. `{home}`, `{cache}` and `{temp}` stand for those folders of the user, so `parent:"{cache}"` selects what is directly inside the user cache folder. |
| `markers` | Required unless the Query names a location with `parent:`. Paths relative to a match, such as `../package.json` (a sibling) or `CACHEDIR.TAG` (inside). A match counts only when one is a plain file (never a link).  |
| `files` | `true` lets files match too, not only folders. |
| `tick` | `false` never pre-ticks the rule. Caches are never pre-ticked either. |
| `platforms` | Any of `linux`, `macos`, `windows`; every platform when left out. |
| `command` | Removes by running this argv, without a shell, in the matched folder, instead of removing the matched paths. The Cleaner shows the exact argv and folder, marks the size Estimated, reports output and a non-zero exit after Clean, and reads the folder's Volume into the Index again. |
| `estimate` | With `command`: a path inside the matched folder whose size is shown. |

Clean checks every match again on disk (the Query, the markers, no links, no Volume mounted inside) before it removes or runs anything.

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
the daily check and a **Check now** button; a failed check only shows a status there.

Where the app can replace itself without admin rights, the update is downloaded in the background, its
minisign signature is checked against the key built into the app, and the row offers **Restart to update**;
nothing is replaced unless the signature matches. This works for the AppImage (when its folder is
writable), the Windows msi (one UAC prompt, then `msiexec` upgrades in place) and portable zip (the exe is
swapped, the old one removed on the next start), and the macOS `.app` (when its folder is writable).
Anywhere else, and when a download fails, the row keeps the **What's new** link. Builds another
tool manages (the AUR package) are compiled without the updater: `cargo build --no-default-features`.

SSH Remotes use your system `ssh` with keys or ssh-agent (no password prompts). S3, FTP, WebDAV
and Azure Blob secrets are kept in the operating system's keyring, never in the settings file.

WebDAV Remotes take a folder URL and a user (Basic authentication; leave the user empty for a
server that needs none) and are read folder by folder with `PROPFIND Depth: 1`, which servers that
refuse `Depth: infinity` accept. Use `https://` anywhere you do not trust the network: Basic
authentication sends the password without encryption over `http://`. Azure Blob Remotes take a storage account, a container
(and optionally a prefix) and either the account key or a SAS token (it needs list permission) as
the secret; fill Endpoint for Azurite or a sovereign cloud. A container is listed flat, like S3.

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

To release, add a `## vX.Y.Z` section to `CHANGELOG.md`, set the same version in the root
`Cargo.toml`, and push the tag `vX.Y.Z`. That section becomes the GitHub release notes.

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
