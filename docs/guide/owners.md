# Owners

An **Owner** is the program, game, package or project a folder belongs to. It is found at one folder, the **Owner root**, and everything beneath that folder has the Owner unless a nearer Owner root sits in between. An Entry has at most one Owner; Entries under no Owner root have none ([ADR-0014](../adr/0014-owners-live-in-the-index.md)).

An Owner has a **kind**:

| Kind | What it is |
| --- | --- |
| System | the operating system and what its package manager installed |
| Program | an application you installed |
| Game | a game installed by a game launcher |
| Project | a code project, recognised by its marker files |

## How Owners are found

Detection runs at the end of every Sync and Catch-up of a Location, inside the same Task. Live updates do not trigger it. The detectors are built in; you can't add your own Owner rules yet.

**On every Location,** Remotes and Offline Volumes included, a folder holding one of these is a Project: `.git`, `Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`, `CMakeLists.txt`, `build.gradle`, `build.gradle.kts`, `Package.swift`, `*.sln` or `*.csproj`. A marker inside a dependency folder (`node_modules`, `vendor`, `site-packages`, `.venv` and similar) does not count.

**On mounted local Volumes,** the platform's own conventions are read as well:

| Platform | Owner root | Kind |
| --- | --- | --- |
| All | `steamapps/appmanifest_*.acf` (the game's install folder) | Game |
| Windows | `C:\Windows` | System |
| Windows | `Program Files` and `Program Files (x86)`: `\<App>` and `\<Vendor>\<App>`; `AppData\Local\Programs\<App>` | Program |
| Linux | `/usr`, `/etc`, `/boot`, `/var/lib`, `/var/cache`, named after your distribution | System |
| Linux | a folder wholly owned by one pacman package, named after the package | System |
| Linux | `/opt/<app>`; Flatpak apps in `/var/lib/flatpak/app`, `~/.local/share/flatpak/app` and `~/.var/app` | Program |
| macOS | `/System`, `/usr` | System |
| macOS | `.app` bundles in `/Applications` and `~/Applications` | Program |

A folder found by a platform convention and by a project marker keeps the platform's Owner.

## Seeing them

- The Explorer's **Selection panel** has an **Owner** row, `<name> · <kind>`, with detail in brackets where there is some. It is left out when the item has no Owner.
- The **Treemap** tooltip adds `Owner: <name> · <kind>`.
- The flat list has an **Owner** column.
- **Colour by**, at the right of the Treemap's legend, switches the tiles between **Category** and **Owner kind**. The choice is remembered. Under Owner kind the legend shows the four kinds and "No Owner" (grey).
- The Dashboard's **Largest Owners** card lists the 10 Owners with the largest Folder Size. Click one to open the Explorer on `owner:"<name>"`. The card is hidden while the Index has no Owners.
- [CSV export](snapshots-and-export.md#export) has `Owner` and `Owner kind` columns.

## Searching

| Write | Matches |
| --- | --- |
| `owner:steam` | Entries whose Owner's name contains the text; ignores case |
| `owner:"Program Files"` | quote a name with spaces |
| `ownerkind:game` | Entries whose Owner is of that kind: `system`, `program`, `game` or `project` |
| `ownerkind:none` | Entries with no Owner |

Both match folders as well as files, and work in the [command line](command-line.md) too. For example, `ve top -n 10 ownerkind:project` ranks the largest folders inside code projects. A Cleanup Rule cannot use them: the check before a Clean reads the disk, where there is no Owner to compare against.
