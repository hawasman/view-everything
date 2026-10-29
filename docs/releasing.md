# Releasing

For maintainers.

## Cut a release

1. Add a `## vX.Y.Z` section to `CHANGELOG.md`. It becomes the GitHub Release notes.
2. Set the same version in the root `Cargo.toml` (`[workspace.package] version`).
3. Commit, then push a signed tag:

   ```sh
   git tag -s vX.Y.Z -m "vX.Y.Z"
   git push origin refs/tags/vX.Y.Z
   ```

The [release workflow](../.github/workflows/release.yml) checks that the tag matches the version, builds Windows (MSI and portable zip), Linux (AppImage) and macOS (universal `.dmg`), then publishes one Release with every file, `SHA256SUMS` and a `.minisig` for each.

A tag push runs the workflow file as it is in the tagged commit. To fix a failed release before anything was published, commit the fix and move the tag to the new commit.

## Signing key (one-time setup)

Release files are signed with a minisign key kept in a GitHub Actions secret. Its public half is `packaging/minisign.pub`, which is also built into the app to check self-updates.

```sh
minisign -G -W -p packaging/minisign.pub -s release.key   # -W: no password, CI cannot type one
gh secret set MINISIGN_SECRET_KEY < release.key            # then keep release.key somewhere safe, not in git
```

Commit the new `packaging/minisign.pub`. The release workflow fails when the secret is missing.

## README screenshots

The images in `docs/images/` come from a demo folder in the UI sandbox, so they show no real files. To redo them for a release:

```sh
cargo build -p ve-app --release
scripts/demo-tree.sh /tmp/demo
printf 'theme = "light"\nupdate_checks = false\n' > /tmp/light.toml
VE_SANDBOX_VOLUME=/tmp/demo VE_SANDBOX_USER=sam VE_SANDBOX_SETTINGS=/tmp/light.toml \
  scripts/ui-sandbox.sh start shots
scripts/ui-sandbox.sh shot shots /tmp/shots/explorer-light.png
scripts/ui-sandbox.sh stop shots
```

Repeat with `theme = "dark"`. Crop each to the 1440×900 window, name it `<screen>-<light|dark>.png`, and run `oxipng -o 4 --strip safe` on it.
