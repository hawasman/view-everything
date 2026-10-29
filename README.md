# view-everything

A desktop app for Linux, Windows and macOS that finds files by name instantly and shows what is
using your disk space. It keeps one Index of every file on your Volumes, so searching and sizing
never wait on a scan.

- **Dashboard**: space per Category, a capacity bar per Location, the largest and most recently
  modified files, and when the Index was last synced.
- **Explorer**: a tree and a flat list beside a Treemap, all driven by one Query. Double-click a
  folder anywhere to zoom into it; the zoom is part of the Query, so you can keep refining it.
- **Locations**: local Volumes are indexed by default. Removable disks stay searchable as Offline
  Volumes after you unplug them, mounted network shares are opt-in, and read-only Remotes can be
  indexed over SSH/SFTP, S3 and FTP.
- **Trash**: Move to Trash and Empty Trash with confirmation.

The vocabulary used in the app and the code is defined in [CONTEXT.md](CONTEXT.md); design
decisions are recorded in [docs/adr](docs/adr).

## Install

Download the latest build from the [Releases](../../releases) page:

| Platform | File |
| --- | --- |
| Windows | `view-everything-windows-x86_64.msi`, or the portable `.zip` |
| Linux | `view-everything-x86_64.AppImage` (`chmod +x`, then run it) |
| macOS (Apple Silicon and Intel) | `view-everything-macos-universal.dmg` |

The macOS app is not notarized: the first time, right-click it in Applications and choose
**Open**.

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

## Development

The workspace has three crates: `ve-core` (Index, Query, Filter), `ve-scan` (scanning Volumes and
Remotes, watching mounts) and `ve-app` (the GPUI app).

Enable the pre-commit hook once per clone; it runs the same checks as CI (rustfmt, clippy with
warnings denied, and the test suite):

```sh
git config core.hooksPath .githooks
```

## Releasing

Bump `version` in the root `Cargo.toml`, commit, then push a matching tag:

```sh
git tag v0.1.0
git push origin v0.1.0
```

The `release` workflow builds every platform and publishes a GitHub Release with the files and a
`SHA256SUMS` list. It refuses a tag that does not match the Cargo version.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option. The bundled Instrument Sans and Instrument Serif fonts are under the SIL Open Font
License (see `crates/ve-app/assets/fonts`).
