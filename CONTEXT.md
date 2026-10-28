# view-everything

A cross-platform desktop app for finding files by name and finding what is consuming disk space, built on one shared record of every file on the user's disks. Its goal is a WizTree-grade GUI that is easier to read and use than existing tools.

## Language

### The Index

**Index**:
The app's persistent record of every Entry on every Indexed Volume. Both Search and the Size Map read the same Index.
_Avoid_: database, cache, catalog

**Entry**:
One file or folder in the Index, with its name, parent folder, Logical Size, On-disk Size and modification time.
_Avoid_: node, item, record, file (when folders are included)

**Volume**:
One mounted filesystem. A Volume is the unit you choose to index; indexing never crosses into another Volume.
_Avoid_: drive, disk, partition, mount (use Volume even on Linux and macOS)

**Indexed Volume**:
A Volume the user has included in the Index. Local fixed Volumes are included by default; removable Volumes only on request. Network and pseudo-filesystems are never included.
_Avoid_: scanned drive, root

**Catch-up**:
Bringing the Index up to date with changes made while the app was not running, done when the app starts.
_Avoid_: rescan, resync, refresh

### Size

**Logical Size**:
An Entry's content length in bytes, as reported by the filesystem.
_Avoid_: file size, apparent size

**On-disk Size**:
The storage actually allocated to an Entry; this is the default sort order. A hard-linked file counts once per Index, no matter how many links point to it.
_Avoid_: allocated size, size on disk, real size

**Folder Size**:
The total of a folder's descendant Entries, in either size measure, with each hard-linked file counted once.
_Avoid_: directory size, total

**Filtered Size**:
The total of a folder's descendant files that count under the current Query: files that match it, and files inside a matching folder unless a stand-alone negated term excludes them. Each hard-linked file is counted once. With an empty Query it equals Folder Size.
_Avoid_: matched size, partial size

**Category**:
A coarse file kind (Video, Image, Audio, Archive, Code, Document, Executable, Other) derived from an Entry's extension; it sets treemap colour and the `type:` filter.
_Avoid_: file type, kind, group

### Views

**Query**:
The search expression that selects which Entries count. Every panel (tree, flat list, treemap, Category breakdown) shows only matching Entries; an empty Query matches everything.
_Avoid_: search, filter, find

**Treemap**:
The panel that draws Filtered Sizes as nested rectangles coloured by Category.
_Avoid_: size map, block view

### Changes

**Trash**:
Moving Entries to the operating system's trash, where they can be restored. Trashed bytes still occupy the Volume.
_Avoid_: delete, remove (unqualified)

**Empty Trash**:
Permanently removing everything in the operating system's trash. The only permanent change the app makes.
_Avoid_: purge, permanent delete
