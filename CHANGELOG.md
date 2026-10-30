# Changelog

## v0.3.0 — unreleased

### One shell for every Module
- The window has a new frame, built to hold the Modules to come (Search, Performance, Apps, Network) without filling up ([ADR-0015](docs/adr/0015-module-rail-inspector-and-timeline-dock.md)): a rail of Modules on the left (Overview; Files with Browse, Cleaner and Duplicates; Settings), one Query box in the top bar that works from every page (`/` focuses it; typing opens Browse), an **Inspector** on the right that shows whatever is selected (an Entry, a Location, a Task) with its facts and actions, a **Timeline** dock at the bottom, and a status strip on every page.
- The Dashboard is now the **Overview**: what is indexed, what can be cleaned up, what is in the Trash and when the Index was last synced, then Locations, Categories, Largest files and Recently modified. Clicking a Location opens it in the Inspector, where Sync now, Open in Browse and Forget live.
- **Duplicates** is its own page under Files, not a tab of the Explorer. **Work** is a page of every Task and every Location, opened from the rail's Work block, which shows the first running Task's progress and the Locations' health; the Tasks and Locations popovers are gone.
- **Compare with** is the Timeline dock: on Browse it shows one tick per Snapshot; click one to compare, Back to Live to stop.
- The Selection panel's facts and actions moved into the Inspector; the Treemap takes the whole right column.
- One set of controls: buttons are flat and square-cornered (one dark primary per screen), tabs are underlined, choices are segmented; cards have a 1px edge and 8px corners; five type sizes; no serif. The chrome carries no colour; teal marks data only (used bars, progress, the Snapshot tick).
- The Index box and the user block left the sidebar; Done Tasks are grey, not green.
- The **Cleaner** shows every Cleanable of a group in one scrolling list; the pager and the per-page choice are gone. Only the rows in view are drawn, so hundreds of thousands of Cleanables scroll smoothly.
- **Duplicates** lists one row per group under its title and facts (groups, copies, wasted space). Clicking a group opens it in the Inspector: its copies (click one to keep it), Open, Show in Browse and Trash others….
- On Browse, **Filters** opens as a column beside the Treemap instead of replacing it, so the Treemap redraws as you narrow the Query.

### Query modifiers
- New Query modifiers: `kind:file` and `kind:folder`; `is:cleanable` (what the Cleaner found), `is:duplicate` (same as `dupe:`) and `is:offline` (Entries on an Offline Volume); `at:/path` (a folder and everything below it); `since:7d`, `since:12h`, `since:2026-10-01` (files modified since then). They combine with text, `|`, `!` and every other modifier.
- A Query that does not parse says why inside the Query box, naming the modifier.
- Cleanup Rules accept `kind:` and `at:` (which names a location, like `parent:`), and refuse `is:` and `dupe:` with the reason.
- `ve` refuses `is:offline` and warns that `is:cleanable` finds nothing.
- `Ctrl+1`, `Ctrl+2` and `Ctrl+3` (`Cmd` on macOS) switch to Overview, Browse and Work from any page, the Query box included.
- The Entry Inspector has **Show in Browse**: it zooms the Query to the folder the Entry is in and selects the Entry there, among its siblings. The rest of the Query stays, unless it would hide the Entry.
- The Overview's finding line says how much of the Cleanable total needs administrator ("…; 1.2 GB of it needs administrator"); with none, it says nothing about it.
- A Location in its first Sync shows Last Synced as "—" everywhere (Overview, Locations, Inspector) until that Sync finishes.

### Owners
- Every folder can now say which program, game, package or project it belongs to. That is its **Owner**, found at an **Owner root** (the game's install folder, the application's folder, the folder holding a project's marker files) and inherited by everything beneath it unless a nearer Owner root sits in between. An Owner has a **kind**: System, Program, Game or Project. Folders under no Owner root have no Owner.
- **Projects** are recognised by their marker files, on every Location, Remotes and Offline Volumes included: `.git`, `Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`, `CMakeLists.txt`, `build.gradle` and `build.gradle.kts`, `Package.swift`, `*.sln` and `*.csproj`. Markers inside dependency folders (`node_modules`, `vendor`, `site-packages`, `.venv` and similar) do not make a Project.
- **Steam games** are read from `steamapps/appmanifest_*.acf`, on every platform.
- **Windows:** `C:\Windows` is System; `Program Files` and `Program Files (x86)` folders (`\<App>` and `\<Vendor>\<App>`) and `AppData\Local\Programs\<App>` are Programs.
- **Linux:** `/usr`, `/etc`, `/boot`, `/var/lib` and `/var/cache` are System, named after your distribution; `/opt/<app>` is a Program; so are Flatpak apps (`/var/lib/flatpak/app`, `~/.local/share/flatpak/app`, `~/.var/app`). A folder wholly owned by one pacman package is System, named after the package.
- **macOS:** `/System` and `/usr` are System; `.app` bundles in `/Applications` and `~/Applications` are Programs.
- Where a platform convention and a project marker find the same folder, the platform's Owner is kept.
- Detection runs at the end of every Sync and Catch-up of a Location, inside the same Task. Steam, Windows, Linux and macOS detection read only mounted local Volumes; project markers are read from the Index alone. Live updates do not trigger it. The detectors are built in; you can't add your own Owner rules yet ([ADR-0014](docs/adr/0014-owners-live-in-the-index.md)).

### Owners in the shell
- The Entry Inspector has an **Owner** row, `<name> · <kind>`, or "—" when the Entry has no Owner.
- The Treemap tooltip adds `Owner: <name> · <kind>`, and Browse's flat list has an **Owner** column.
- **Colour by: Category | Owner kind**, under the Treemap's title, is remembered between runs. Under Owner kind, the legend shows the four kinds and "No Owner" (grey).
- Two new Query modifiers: `owner:<text>` matches an Owner's name (a substring, ignoring case; quote a name with spaces: `owner:"Program Files"`), and `ownerkind:system|program|game|project|none`. Both match folders as well as files, combine with the rest of a Query, and work in `ve find` and `ve top`.
- CSV export gains `Owner` and `Owner kind` columns, in the app, in `ve export` and in `ve find --csv`.
- The Overview has a **Largest Owners** panel: the 5 Owners with the largest Folder Size. Clicking one shows its Owner root in the Inspector. The panel is hidden while the Index has no Owners.

### Index format
- The Index is now format 8. A format 7 file is converted when loaded, and every Location is detected once at launch, so Owners appear without waiting for a Sync.
- A file saved by v0.3 can't be read by v0.2.

### Engine
- The engine (the Index, Tasks, Locations, Live updates, Sync scheduling and Snapshots) is its own crate, `ve-engine`, with no GUI in it ([ADR-0016](docs/adr/0016-engine-library-in-the-tray-process-with-a-local-socket.md)). The app hosts it and behaves as before.
- A finished Sync's Task shows the Entries it read, as its result says; a quick Sync used to show 0.

## v0.2.1

### Half the memory
- The Index takes about half the memory it did: on a 5.6M-Entry Index, 211 MiB instead of 424 MiB, and the app sits at about 270 MiB after a Sync instead of 400 MiB. Each distinct name is stored once (1.8M names for 5.6M Entries), a folder's children are one run of ids rather than a list, and On-disk Size and creation time are stored only where they differ from the usual (Logical Size rounded to 4 KiB; the modification time). The saved file is 208 MB instead of 470 MB and loads in a quarter of a second.
- Times are kept to the second. The JSON export's `modified_ns` field stays, as whole seconds.
- Saving the Index no longer makes a full copy of it first, so memory no longer doubles for a moment at every save.
- An Index saved by v0.2 is converted when loaded, without a new Sync.

### Smooth while syncing
- The window no longer stutters while a Sync runs. Each frame read the Index behind every walker thread that had a folder ready to write, up to 0.7 s on a 24-thread machine; writers now queue on a gate, so a frame waits for one write at most.
- `VE_FRAME_TRACE=<ms>` prints the views whose render took longer than that, for finding slow frames.

### Fixes
- The Treemap draws the whole Index, one tile per Location, when the Query matches Entries on more than one Location or is empty, so it shows what the tree shows. An Offline Volume's tile is marked "(offline)". The Index crumb and Zoom out reach that level.
- Reopening the window from the tray brings back the Query (in the box and the Filters button), the page and tab, the Compare with date, and the tree's open folders and selection.
- The Cleaner shows a spinner while it looks for Cleanables, in the list and on the Dashboard card, and says it once.

## v0.2.0

### Know what the app is doing
- Background work (Sync, Duplicate search, Cleaner search, Clean, Trash, Empty Trash, Compare) now shows up as a **Task**. The control that started it spins and can't be pressed twice, and the sidebar lists running Tasks and the last 10 finished ones with their outcome, duration and a Retry for failures.
- Tasks with a known total show a progress bar; the rest show a spinner and a live count.
- Sync, Duplicate search, Cleaner search and Compare can be cancelled. Cancelling a Sync leaves the Location's earlier Entries in place.

### Query
- An ✕ in the Query box clears it. Escape first drops the search terms and keeps the folder you zoomed into, then clears the rest.
- The Query waits for a short pause in typing before searching.
- The Filters panel has a Reset that removes only the clauses it added.

### Treemap
- Folders carry a header strip with their name and size, down to an adjustable **Depth** (default 3).
- Hovering outlines every folder the tile sits in, and the tooltip shows the full path with the size of each enclosing folder.
- Restyled tiles. A folder shows its 96 largest children and merges the rest into one "N more" tile. Fixed a crash on very large folders.
- A Selection panel under the map shows the selected item's size, share, contents and last write, with Open, Show in Explorer, Trash and Sync this folder.
- Zooming glides from the old layout to the new one, and hover outlines fade. Turn on **Reduce motion** in Settings, or in your OS, to switch motion off.

### Sync now
- **Sync now** on the Dashboard and in the tray menu re-reads every online Volume in full, even when Live updates are following it (on NTFS, a full MFT read).
- Each Location has its own Sync now on the Dashboard and in Settings. Remotes sync only from there.
- **Sync this folder** in the Explorer's right-click menu and the Selection panel re-reads one folder. Inside a Remote it works over SSH and SFTP; other Remotes sync whole.

### Locations and Remotes
- A Locations popover, with a health dot, shows each Location's state and lets you Sync it.
- A Remote's name is optional; it falls back to its host or bucket.

### Explorer
- Select several rows (click, Ctrl/Shift-click). The right-click menu offers Open, Show in Explorer, Open terminal here, Copy path(s), Trash, Sync this folder and Export selection to CSV.
- The table uses the available width better.

### Cleaner
- The Cleaner shows progress while it searches and cleans, and Trash has its own icon.

### Tray and start at login
- The app can keep running in the system tray after its window closes (Settings → Background; off by default). The tray menu has Open, Sync now, the running Tasks and Quit.
- Quit saves the Index after the window and tray are hidden, so closing feels instant.
- **Start at login** starts the app straight into the tray. On Windows it uses a scheduled task, so there is one administrator prompt when you turn it on and none at login. On Linux, a login start leaves Live updates paused until you start them, so you aren't asked for a password at login.
- On GNOME the tray icon needs the AppIndicator extension; without a tray the app quits when its window closes, as before.

### Duplicates
- **Duplicate Exclusions** (Settings → Duplicates) leave dependency, build, version-control and package-cache folders (`node_modules`, `target`, `.venv`, `.git` and similar) out of the search, since their copies are there by design. The panel says how many files were skipped.
- The Duplicates panel follows the Query: a group shows if any of its copies matches.

### Faster
- Results for a new Query arrive several times faster on large Indexes (5M Entries: under 100 ms instead of up to 400 ms).
- Treemap hover is instant on maps with thousands of tiles, and the Treemap and Cleaner do less work while nothing changes.
