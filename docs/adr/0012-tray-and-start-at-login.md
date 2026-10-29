# Tray and start at login, still no privileged service

Keeping the app running is a foundation, not a convenience. Planned features (network and CPU/RAM/GPU monitoring with history, and a global search hotkey in the style of PowerToys Run) only work while the app is running.

This amends ADR-0003. The app can now keep running after its window closes, in the system tray, and can start at login, so the Index stays fresh and Live updates keep running between uses. It still installs no privileged service. A service would remove the elevation prompts (UAC to read the NTFS MFT, pkexec for the Linux `fanotify` helper), but it would make the GUI a client of a headless indexer, with IPC, ownership of the Index file and per-platform installers. That is deferred, not rejected.

## Consequences

- GPUI has no tray icon API and no way to hide a window. "Closing to the tray" closes the window, and the process keeps running with no window open. The Engine therefore lives at app level, not in the window.
- Closing the window still quits the app unless the user turns on keeping it in the tray.
- Starting at login inherits the elevation problem: an elevated start on Windows, or Live updates on Linux, would prompt at every login.
