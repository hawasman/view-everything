# Snapshots, Compare with and Export

## Snapshots

A Snapshot is the Folder Size of every folder, by path, at one moment ([ADR-0010](../adr/0010-snapshots-as-gzipped-columns-beside-the-index.md)).

- **When:** the Index saves one on its first save each day, once every Location has synced.
- **Where:** in a `snapshots/` folder beside `index.bin`.
- **How long:** the last 30 days are kept, then the first Snapshot of each month.

## Compare with

**Compare with** in the Explorer tree picks a Snapshot date and adds a sortable **Change** column. The comparison runs in the background as a Task.

| Row | Change shows |
| --- | --- |
| a folder in both | its growth or shrinkage: On-disk Size now minus its size in the Snapshot |
| a folder that did not exist then | `new` |
| a folder that is gone | a muted `(removed)` row under its parent |

## Export

**Export CSV** writes what the flat list shows: every Entry matching the Query, including a folder you zoomed into.
- Columns: path, name, Logical Size, On-disk Size, modified (UTC), Category, [Owner](owners.md) and Owner kind.
- A name a spreadsheet could read as a formula (starting with `=`, `+`, `-`, `@`, tab or CR) gets a leading `'`.
- **Export selection to CSV** in the right-click menu writes only the selected rows.

**Export PNG** on the Treemap saves the tiles as drawn, at 2x, in the current theme's colours, without the text labels.

Both ask where to save. Where there is no save dialog, they use the Downloads folder.

From the command line, [`ve export`](command-line.md) writes the same CSV.
