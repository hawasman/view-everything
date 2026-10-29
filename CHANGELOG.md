# Changelog

## v0.2.0

### Know what the app is doing
- Background work (Sync, Duplicate search, Cleaner search, Clean, Trash, Empty Trash, Compare) now shows up as a **Task**. The control that started it spins and can't be pressed twice, and the sidebar lists running Tasks and the last 10 finished ones with their outcome, duration and a Retry for failures.
- Tasks with a known total show a progress bar; the rest show a spinner and a live count.
- Sync, Duplicate search, Cleaner search and Compare can be cancelled. Cancelling a Sync leaves the Location's earlier Entries in place.

### Query
- An ✕ in the Query box clears it. Escape first drops the search terms and keeps the folder you zoomed into, then clears the rest.
- The Filters panel has a Reset that removes only the clauses it added.

### Treemap
- Folders carry a header strip with their name and size, down to an adjustable **Depth** (default 3).
- Hovering outlines every folder the tile sits in, and the tooltip shows the full path with the size of each enclosing folder.
- A Selection panel under the map shows the selected item's size, share, contents and last write, with Open, Show in Explorer, Trash and Sync this folder.
- Zooming glides from the old layout to the new one, and hover outlines fade. Turn on **Reduce motion** in Settings, or in your OS, to switch motion off.

### Sync now
- **Sync now** in the sidebar re-reads every online Volume in full, even when Live updates are following it (on NTFS, a full MFT read).
- Each Location has its own Sync now on the Dashboard and in Settings. Remotes sync only from there.
- **Sync this folder** in the Explorer's right-click menu and the Selection panel re-reads one folder. Inside a Remote it works over SSH and SFTP; other Remotes sync whole.

### Tray and start at login
- The app can keep running in the system tray after its window closes (Settings → Background; off by default). The tray menu has Open, Sync now, the running Tasks and Quit.
- **Start at login** starts the app straight into the tray. On Windows it uses a scheduled task, so there is one administrator prompt when you turn it on and none at login. On Linux, a login start leaves Live updates paused until you start them, so you aren't asked for a password at login.
- On GNOME the tray icon needs the AppIndicator extension; without a tray the app quits when its window closes, as before.

### Duplicates
- **Duplicate Exclusions** (Settings → Duplicates) leave dependency, build, version-control and package-cache folders (`node_modules`, `target`, `.venv`, `.git` and similar) out of the search, since their copies are there by design. The panel says how many files were skipped.
- The Duplicates panel follows the Query: a group shows if any of its copies matches.

### Faster
- Results for a new Query arrive several times faster on large Indexes (5M Entries: under 100 ms instead of up to 400 ms).
- Treemap hover is instant on maps with thousands of tiles, and the Treemap and Cleaner do less work while nothing changes.
