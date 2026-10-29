# Clean removes permanently instead of using the Trash

Before this, Empty Trash was the only permanent change the app made. Clean, run from the Cleaner, is a second one: it removes the Cleanables in a reviewed Cleanup Plan without going through the Trash, because trashed bytes still occupy the Volume and moving caches or build artefacts to the Trash would free nothing. The safety comes from the Cleanup Plan instead:

- Cleanables are found from the Index and checked again on disk before removal.
- A build artefact counts only when a project marker is found, such as a sibling `Cargo.toml` or `package.json`, or a genuine `CACHEDIR.TAG` inside. Markers must be plain files, never links. Removal never follows links and refuses a folder with a Volume mounted on or inside it.
- The advanced view shows every path and command exactly.
- Command rules are marked Estimated and run without a shell.
- Clean removes only what the user owns, judged by walking the whole Cleanable (links not followed); where ownership cannot be told it counts as not theirs. Everything else is Admin only: it gets a separate admin step that shows its exact command first and runs it with `pkexec` (Linux only; elsewhere the command is shown for the user to run). Command rules are never run elevated. On Windows only what lies under the Windows folder is treated as Admin only, since there is no cheap owner test.
