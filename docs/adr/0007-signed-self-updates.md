# Self-updates verified with a minisign signature

The app checks GitHub Releases at most once a day, downloads and verifies an update in the background, and swaps it in only when the user clicks "Restart to update". Each platform swaps differently: the AppImage file, `msiexec` for the msi, a rename of the running exe for the portable zip, and the `.app` bundle on macOS. Every release asset is signed with a minisign (ed25519) key kept in a GitHub Actions secret, and the public key is built into the app. `SHA256SUMS` alone would not do: it ships in the same release, so a compromised GitHub account could replace both the asset and its checksum.

Installs that another tool manages do not update themselves. The AUR build leaves the updater out, and the AppImage embeds `gh-releases-zsync` update information so Shelly, AppImageUpdate and Gear Lever can update it in their own way.
