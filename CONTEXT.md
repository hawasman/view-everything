# view-everything

A cross-platform desktop app for finding files by name and finding what is consuming disk space, built on one shared record of every file on the user's disks and remote storage. Its goal is a WizTree-grade GUI that is easier to read and use than existing tools.

## Language

### The Index

**Index**:
The app's persistent record of every Entry in every Location. Both Search and the Size Map read the same Index.
_Avoid_: database, cache, catalog

**Entry**:
One file or folder in the Index, with its name, parent folder, Logical Size, On-disk Size and modification time.
_Avoid_: node, item, record, file (when folders are included)

**Location**:
Anything whose Entries are in the Index: a Volume or a Remote.
_Avoid_: source, root, storage

**Volume**:
One mounted filesystem, local or a mounted network share. A Volume is the unit you choose to index; indexing never crosses into another Volume. A Volume is recognised by the filesystem itself, so it stays the same Volume when mounted at a different path.
_Avoid_: drive, disk, partition, mount (use Volume even on Linux and macOS)

**Indexed Volume**:
A Volume the user has included in the Index. Local fixed Volumes are included by default; removable Volumes and mounted network shares only on request. Pseudo-filesystems are never included.
_Avoid_: scanned drive, root

**Offline Volume**:
An Indexed Volume that is not currently mounted. Its Entries stay in the Index and can be searched and sized, but not changed.
_Avoid_: missing drive, disconnected, unplugged

**Remote**:
Storage reached over a network protocol (S3, FTP, WebDAV, Azure Blob, SFTP or SSH) without mounting it. Remotes are read-only: the app never changes or removes their Entries, and they have no Trash.
_Avoid_: cloud, network drive, server, share

**Last Synced**:
When a Location was last brought fully up to date. The app's overall Last Synced is that of its least recently synced Location.
_Avoid_: last updated, last scan, last refresh

**Catch-up**:
Bringing a Location up to date with changes made while the app was not running, done at launch for Locations whose Last Synced is stale.
_Avoid_: rescan, resync, refresh

**Snapshot**:
The Folder Size of every folder in the Index at one moment, kept so two moments can be compared.
_Avoid_: scan history, backup, version

**Compare with**:
The Explorer choice of a Snapshot date. The tree then gains a **Change** column: each folder's On-disk Size now minus its Size in the Snapshot, `new` for a folder the Snapshot lacked, and a muted `(removed)` row under the parent of a folder it had that is gone.

### Size

**Logical Size**:
An Entry's content length in bytes, as reported by the filesystem.
_Avoid_: file size, apparent size

**On-disk Size**:
The storage actually allocated to an Entry; this is the default sort order. A hard-linked file counts once per Index, no matter how many links point to it. Where a Remote cannot report allocation, it equals Logical Size.
_Avoid_: allocated size, size on disk, real size

**Unindexed Space**:
Space a Volume reports as used that no Entry accounts for: unreadable folders, filesystem metadata, snapshots.
_Avoid_: missing space, other, overhead

**Folder Size**:
The total of a folder's descendant Entries, in either size measure, with each hard-linked file counted once.
_Avoid_: directory size, total

**Filtered Size**:
The total of a folder's descendant files that count under the current Query: files that match it, and files inside a matching folder unless a stand-alone negated term excludes them. Each hard-linked file is counted once. With an empty Query it equals Folder Size.
_Avoid_: matched size, partial size

**Category**:
A coarse file kind (Video, Image, Audio, Archive, Code, Document, Executable, Other) derived from an Entry's extension; it sets treemap colour and the `type:` filter.
_Avoid_: file type, kind, group

**Duplicate**:
A file on a Volume whose content is identical to another file's, confirmed by hash. Hard links to one file are not Duplicates.
_Avoid_: copy, twin, same file

### Views

**Dashboard**:
The home view: a glance at the state of the Index, its Locations and their Last Synced.
_Avoid_: home, overview, summary

**Explorer**:
The view for digging into Entries: the tree and flat lists, the Treemap and the Category breakdown, all following the Query.
_Avoid_: browser, main view, file view

**Query**:
The search expression that selects which Entries count. Every panel in the Explorer (tree, flat list, Treemap, Category breakdown) shows only matching Entries; an empty Query matches everything.
_Avoid_: search, filter, find

**Saved Query**:
A Query kept under a name in the settings, to apply again from the Explorer. Saving under an existing name (ignoring case) replaces that Query.

**Treemap**:
The panel that draws Filtered Sizes as nested rectangles coloured by Category. It draws the folder everything the Query shows sits in; zooming into a folder (on the Treemap or in a list) adds that folder to the Query, so the tree and flat list follow and the Query can still be refined.
_Avoid_: size map, block view

### Changes

**Trash**:
Moving Entries to the operating system's trash, where they can be restored. Trashed bytes still occupy the Volume.
_Avoid_: delete, remove (unqualified)

**Empty Trash**:
Permanently removing everything in the operating system's trash. One of the app's two permanent changes, with Clean.
_Avoid_: purge, permanent delete

### Cleaning

**Cleaner**:
The view for reclaiming space from files that can be regenerated or discarded: temporary files, caches and build artefacts.
_Avoid_: cleanup window, optimizer

**Cleanup Rule**:
A Query, the project markers that must be present (or a location the Query names), and how to remove what matches: either the matched paths themselves or a tool's own command. Each rule files its Cleanables under Temporary Files, Caches or Build Artefacts, which decides the age limit for pre-ticking. Built-in and user-written rules are the same kind of thing.
_Avoid_: action, preset, Category

**Cleanable**:
One thing a Cleanup Rule found, such as a project's build-artefact folder.
_Avoid_: junk, candidate, finding

**Cleanup Plan**:
The reviewed list of Cleanables, with the exact paths and commands that Clean will apply.
_Avoid_: preview, report

**Clean**:
Permanently removing the Cleanables in a Cleanup Plan, bypassing the Trash so the space is actually freed.
_Avoid_: delete, purge, wipe
