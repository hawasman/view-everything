# Owners are stored in the Index, on their Owner roots only

An Owner (CONTEXT.md) is the program, game, package or project a folder belongs to. It is found at one folder, the Owner root, and inherited by everything below it; an Entry has at most one Owner. v0.3 shows it in the Explorer, colours the Treemap by Owner kind, and filters with `owner:` and `ownerkind:`.

Owners live in the Index (format 8), not beside it:

- An `owners` table holds each Owner's kind and name, and a `SparseColumn` over Entry rows holds an owner id on Owner roots only. A few thousand roots on a multi-million-Entry Index cost kilobytes; the column type already exists for On-disk Size and creation time (ADR-0013).
- An Entry's Owner is found by walking `parent` until a row with an owner id is met, the same walk the Cleaner's marker check does. Nothing is stored per descendant, so a Sync that adds or removes a marker changes one row.
- Detection runs as the last step of every Sync and Catch-up of a Location, inside that Task: project markers from the Index alone (every Location, Remotes and Offline Volumes included), platform detectors (Steam manifests, Flatpak directories, the package database, `Program Files`, `.app` bundles) only on mounted local Volumes. Live updates and journal polls do not retrigger it. It runs in three steps so that no file is read while the Index is locked: a plan under the read lock, the disk reads with no lock, and the result applied under the write lock, skipping any root removed in between.

The alternative was a side file like Snapshots (ADR-0010). It was rejected because every reader of the Index would need a second loader: the CLI opens `index.bin` directly and already evaluates Queries against it, CSV export reads the Index, and a side file can drift from the Entries it names between a Sync and its save. Snapshots are a different case: they are history, read by one view, and never consulted by a Query.

Cost: a format bump. A format 7 file loads with an empty `owners` table and every Location is detected once at that launch, so Owners are complete before the first Sync; a v0.3 file is unreadable by v0.2, as every format bump has been.
