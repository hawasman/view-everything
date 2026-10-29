# Duplicates

The Explorer's **Duplicates** tab groups files with identical content, largest wasted space first. In a Query, `dupe:` matches the same files.

## Using it

1. Open the Duplicates tab. The search runs in the background as a Task, with progress, and can be cancelled.
2. In each group, pick the Duplicate to keep.
3. **Trash others** moves the rest to the Trash after a confirmation. The Index and the groups update.

The panel follows the Query: a group shows if any of its copies matches. The list is virtualised, so any number of groups or copies stays light.

## Duplicate Exclusions

**Settings → Duplicates** leaves out folders whose copies are there by design: dependency, build, version-control and package-cache folders such as `node_modules`, `target`, `.venv` and `.git`. The panel says how many files were skipped.

## What counts as a Duplicate

Files on a local Volume with identical content, confirmed by hash.
- Hard links to one file are not Duplicates, and only one name of a hard-linked file is listed.
- Remotes and Offline Volumes are never hashed.

## How files are compared

Each step only looks at the files that are still candidates after the step before:

1. files of at least 1 MB with an equal size;
2. equal first and last 64 KB;
3. equal BLAKE3 hash of the whole file.

Hashing is careful with your disk and your files:
- It reads one file at a time per Volume.
- It opens only regular files, never a symlink, FIFO, socket or device, even if one replaced a file since it was indexed.
- It reads at most the size the Index recorded, and drops a file that changed while it was hashed.
- It is redone for Index changes at most every 5 minutes.
- Hashes are cached in `hashes.bin` beside the Index until a file's size or modification time changes.

The [command line](command-line.md) does not read `hashes.bin`, so `dupe:` finds nothing there.
