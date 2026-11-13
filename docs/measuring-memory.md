# Measuring memory

How the numbers in [#31](https://github.com/hawasman/view-everything/issues/31) (results from #68) were taken, so they can be taken again after a fix. Use release builds; debug builds hold different amounts.

## A large Index

```bash
cargo build -p ve-app --release --features bench --examples
target/release/examples/gen_index 5000000 /path/to/scratch/index-5m.bin synthetic
```

- **Where the Index goes:** a scratch folder, never the real Index in `~/.local/share/view-everything`.
- **The identity argument:** with `synthetic`, the Index-sized Volume matches no mounted Volume, so it stays an Offline Volume next to whatever the app indexes. Without it the identity is `bench`, which `VE_BENCH_MOUNT` matches.
- **What the Index looks like:** 50k folders of 100 files each. All names differ and all sizes differ, and every file's creation time differs from its modification time. That makes names and `created` worst cases; real names are interned and share sizes.

## Headless: the Index and each cache

```bash
target/release/examples/mem_report /path/to/scratch/index-5m.bin [/path/to/a/real/folder]
```

- **What it measures:** it loads the Index, then builds what the engine keeps beside it: FileOrder, a View for the empty Query and for `ext:mp4`, Category sums and the Duplicates search.
- **With a folder:** it also adds the folder as a second Location and Syncs it, to show what a Sync leaves on top of a loaded Index.
- **The table:** each row gives the heap bytes held, the peak while building, live heap, and RSS before and after `malloc_trim`. A counting global allocator supplies the heap figures.
- **The column list:** after the table it prints every allocation of 256 KiB or more that `Index::load` left live, in the order each was first made. Deserialization follows the order of `Index`'s fields, so the list reads as the columns in that order. A column of several vectors (`SparseColumn`, `Rank`, `U64Column`) shows as several lines; the small ones (the wide side tables, the folder child runs) are not listed.
- **Heap is not RSS:** heap counts capacity. Vectors grown by doubling hold pages that are never touched, so RSS is the figure for what the machine pays.
- **A real folder with Duplicates:** sparse files are enough. `truncate -s 2M f && printf group-1 | dd of=f conv=notrunc` gives a 2 MiB file that costs no disk.

## The app

The app runs in the UI sandbox (see `AGENTS.md`), with a `VE_BIN` wrapper script that:

- copies the scratch Index to `$XDG_DATA_HOME/view-everything/index.bin` before starting the app;
- samples `/proc/<pid>/status` every second: `VmHWM`, `VmRSS`, `RssAnon`, `RssFile`, `RssShmem` and `VmSwap`.

**What to compare:** compare `RssAnon + VmSwap`, not `VmRSS`. The sandbox's cgroup and a busy host swap the app out while it is idle. That moved `VmRSS` by hundreds of MiB without the app doing anything.

**`VE_SANDBOX_VOLUME`:** this makes a real folder the only Volume, so Sync and Catch-up run. Two catches:

- **Paths under `/home`:** the folder is bind-mounted under a fresh `/home`, so the folder, the wrapper and the Index must live outside `/home` (for example `/tmp`).
- **`VE_BENCH_MOUNT`:** the bench build's `VE_BENCH_MOUNT` turns every Sync off. Use it only for a launch with no Sync at all.

**A Sync on purpose:** add files to the folder, open the Location in Tasks and press Sync now.

**Attribution:** use jemalloc heap profiling with the `profiling` build, so the profile has symbols:

```bash
cargo build -p ve-app --profile profiling --features bench
# in the wrapper:
LD_PRELOAD=/usr/lib/libjemalloc.so.2 \
MALLOC_CONF=prof:true,prof_active:true,lg_prof_sample:18,lg_prof_interval:28,prof_prefix:/tmp/ve/ve
jeprof --text --cum --show_bytes target/profiling/view-everything /tmp/ve/ve.<pid>.<n>.i<n>.heap
```

- **Picking dumps:** a dump is written every 256 MiB allocated. The largest total is the peak; the last one is the app at rest.
- **What the profile counts:** allocated capacity, as the headless heap figures do.
