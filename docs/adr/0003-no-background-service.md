# No background service; Catch-up at launch when stale

_Partly superseded by ADR-0009 (live change tracking on macOS and Linux)._

Unlike Everything, the app installs no privileged service that starts at boot. It updates the Index only while running, and at launch it shows the saved Index at once with each Location's Last Synced time. A privileged service would bring its own install, update and security problems.

Catch-up at launch runs only for Locations whose Last Synced is older than the refresh interval, so restarting the app does not repeat a full re-walk it just did. NTFS Volumes are the exception: reading the change journal is cheap, so they always catch up. On other filesystems the folder tree is re-walked and the differences applied to the saved Index.

The cost is an Index that can be stale after launch, by up to one refresh interval, and that is labelled as such. On Linux, changes are noticed only when a re-walk runs (every few minutes, set in the Volumes settings), because whole-disk `fanotify` needs root. The re-walk covers every folder, not only those whose modification time changed: appending to a file doesn't change its folder's modification time. A polkit helper for Linux is a possible later addition.
