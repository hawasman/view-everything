# Snapshots are gzipped columns in a folder beside the Index

A Snapshot is the Folder Size of every folder, keyed by path, because Entry ids change on every save. It is not part of `index.bin`: the first save of each day writes `snapshots/YYYY-MM-DD.snap` beside it, so the Index stays one file and a Snapshot can be deleted, or kept for a month, on its own. Retention (the last 30 days plus the first Snapshot of each month) is decided by listing the folder, so there is no catalogue to keep in step.

A Snapshot stores folders as columns (the position of the parent, a name, both sizes), not as paths, then bincode and gzip them with `flate2`. `flate2` was already in the build through `ureq`, so no new crate is added; `Compression::fast` is enough because names repeat heavily. Measured on a real Index of 6M Entries, 568k of them folders, this is 5.2 MB per day (about 160 MB for the 30 daily Snapshots), and the write takes about 0.45 s on its own thread. Only reading the folders (about 90 ms at that size) holds the Index's read lock.

No Snapshot is taken until every Location has synced once: one made during the first scan is partial, and comparing with it would report everything the scan had not yet reached as new.
