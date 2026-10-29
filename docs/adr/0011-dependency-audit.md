# Dependencies are audited with cargo-audit; four kinds of "unmaintained" stay

`cargo audit` (RustSec) must report no vulnerabilities. The zip crate was the last one: 2.6.1 carried RUSTSEC-2026-0367/0368/0369 and GHSA-m665-v7pm-7grj, and is now 9.1.3. The self-updater never calls `ZipArchive::extract`: it reads the one entry named exactly `view-everything.exe` into memory and refuses it if it is a symlink, so a downloaded zip cannot write anywhere. Tests cover `..` names, absolute names and a symlink entry.

Nine "unmaintained" warnings remain. None is a vulnerability, and each has a reason to stay:

- **bincode 1.3.3** (RUSTSEC-2025-0141), used directly by ve-core and ve-scan for the Index and the duplicates cache. It is finished software, and its latest release, 3.0.0, is a `compile_error!` tombstone. Replacing it changes the on-disk Index format that ADR-0001 shares between sessions, for no security gain. Revisit if a maintained serde binary format is wanted for another reason.
- **instant, paste, rustybuzz, ttf-parser, zbus_names, zvariant, zvariant_derive, rustls-pemfile**, all pulled in through GPUI (`gpui-pre` 0.3.10, `gpui-kit` 0.8.1, the latest pair). We cannot replace them without leaving GPUI; they go when GPUI moves off them.

Re-run `cargo audit` when bumping GPUI and before each release.
