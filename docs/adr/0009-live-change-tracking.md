# Live change tracking on macOS and Linux

This replaces part of ADR-0003. On macOS the app uses FSEvents, which needs no root and can replay the changes made since the app last ran, so Catch-up becomes cheap as it is on NTFS. On Linux, inotify would need one watch per folder, which means millions here, so the app instead offers an opt-in `fanotify` helper. The helper is started with pkexec while the app runs and stops with it; it is not a boot-time service, so ADR-0003's "no background service" still holds. Without the helper, Linux keeps the periodic re-walk.

## Implementation notes

- macOS: one FSEvents stream over the local Volumes, delivered on a dispatch queue. Its event id is saved beside the Index (`fsevents-id`) with each save; a history the system drops or wraps arrives as flagged events, which re-walk the folder or, when the ids wrapped, every Volume.
- Linux: `ve-fanotify` marks whole filesystems (`FAN_MARK_FILESYSTEM`, `FAN_REPORT_DFID_NAME`, which needs kernel 5.9) and runs as root, because that and `open_by_handle_at` need `CAP_SYS_ADMIN` and `CAP_DAC_READ_SEARCH`. It prints changed paths to a pipe, reads `mark <mount>` lines for Volumes mounted later, and exits when stdin ends or its parent dies, so it cannot outlive the app. It opens files only with `O_PATH` and writes none.
- Both feed one coalescer; a burst becomes a re-listing of each changed folder (one level, plus new subfolders in full) through the same code as the walk. While a source runs, its Volumes are not re-walked periodically, and their Last Synced keeps up. If the helper ends on its own, every Volume is re-walked at once and the reason is shown.

