# Clean removes permanently instead of using the Trash

Before this, Empty Trash was the only permanent change the app made. Clean, run from the Cleaner, is a second one: it removes the Cleanables in a reviewed Cleanup Plan without going through the Trash, because trashed bytes still occupy the Volume and moving caches or build artefacts to the Trash would free nothing. The safety comes from the Cleanup Plan instead:

- Cleanables are found from the Index and checked again on disk before removal.
- A build artefact counts only when a project marker is found, such as a sibling `Cargo.toml` or `package.json`.
- The advanced view shows every path and command exactly.
- Command rules are marked Estimated and run without a shell.
- Version 1 removes only files the user owns. Files that need admin rights get a separate admin step that shows its command first.
