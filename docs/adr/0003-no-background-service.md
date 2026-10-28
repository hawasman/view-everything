# No background service; Catch-up at launch

Unlike Everything, the app installs no privileged service that starts at boot. It updates the Index only while running, and does a Catch-up at launch. On Windows that Catch-up reads the NTFS change journal. Elsewhere, the folder tree is re-walked and the differences applied to the saved Index. A privileged service would bring its own install, update and security problems.

The cost is an Index that is briefly stale after launch. On Linux, changes are noticed only when a re-walk runs (every few minutes, set in the Volumes dialog), because whole-disk `fanotify` needs root. The re-walk covers every folder, not only those whose modification time changed: appending to a file doesn't change its folder's modification time. A polkit helper for Linux is a possible later addition.
