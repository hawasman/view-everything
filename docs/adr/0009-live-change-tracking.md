# Live change tracking on macOS and Linux

This replaces part of ADR-0003. On macOS the app uses FSEvents, which needs no root and can replay the changes made since the app last ran, so Catch-up becomes cheap as it is on NTFS. On Linux, inotify would need one watch per folder, which means millions here, so the app instead offers an opt-in `fanotify` helper. The helper is started with pkexec while the app runs and stops with it; it is not a boot-time service, so ADR-0003's "no background service" still holds. Without the helper, Linux keeps the periodic re-walk.
