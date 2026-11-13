# Staying current: Sync, Live updates and Background

The Index is saved between runs, so search and sizes never wait on a scan. Four mechanisms keep it up to date.

| Mechanism | When it runs |
| --- | --- |
| **Catch-up** | at launch, for Locations whose Last Synced is stale |
| **Periodic Sync** | on each Location's schedule: every 30 minutes for local Volumes, 120 for network shares, Manual for Remotes (all adjustable in Settings) |
| **Sync now** | when you ask: for every Volume (tray), one Location (the Inspector, Settings) or one folder (**Sync this folder** in Browse's right-click menu and the Inspector) |
| **Live updates** | continuously while the app runs, where the platform allows it |

Sync now re-reads in full, even a Volume Live updates is following. Every Sync shows up as a Task and can be cancelled. A cancelled Sync leaves the Location's earlier Entries in place.

## Who started a Task

Each Task on the Work page says who started it. The ones the app starts on its own (Catch-up, a periodic Sync, a Duplicate search, a Cleaner search) run at the operating system's lowest CPU and disk priority, whether the window is open or not, so they give way to whatever else the machine is doing: on Linux the idle scheduling class and the idle I/O class, on Windows background processing mode, on macOS the background band. The ones you start (Sync now, Sync this folder, Rescan, Clean, Trash) run at full speed. The Inspector shows a Task's priority.

## Live updates by platform

| Platform | Live updates | Catch-up at launch |
| --- | --- | --- |
| macOS | FSEvents, on by default | replays only the changes since the event id saved with the Index |
| Linux | opt-in helper that runs as root (below) | re-walk |
| Windows | not available; periodic Sync instead | from the NTFS change journal when run as administrator, otherwise a re-walk |

With Live updates, a file created, renamed, resized or deleted shows within seconds ([ADR-0009](../adr/0009-live-change-tracking.md)). On macOS, if the system refuses or has dropped the change history, the Location is re-walked.

### Linux: the `ve-fanotify` helper

Following a whole disk on Linux needs root, so it is opt-in. **Settings → Live updates** starts a small helper, `ve-fanotify`, with `pkexec` (one password prompt).
- It runs only while the app runs, and stops when the app does or when you switch it off.
- It only reports which paths changed, and never writes anything.
- It is installed by the AUR package in `/usr/libexec/view-everything/`. It cannot work from the AppImage, because root cannot read inside it.
- If you deny the password prompt, the app says so and keeps syncing on its schedule, as it does without the helper.
- After a reboot, Linux catches up by re-walking.

### Windows: administrator rights

Started as administrator, the app reads each local NTFS Volume's Master File Table directly instead of walking its folders, then catches up through the change journal. On launch it asks once (UAC); decline, or start it with `--no-elevate`, and it walks folders instead.

## Background: tray and start at login

**Settings → Background** has two switches, both off by default ([ADR-0012](../adr/0012-tray-and-start-at-login.md)).

**Keep running in the tray.** Closing the window leaves the app in the system tray, so the Index stays fresh and Live updates and Tasks keep running. The tray menu has Open, Sync now, the running Tasks and Quit.

**Start at login.** The app starts straight into the tray when you log in.

| Platform | How | Prompts |
| --- | --- | --- |
| Linux | XDG autostart entry | none. Live updates stay paused after a login start, so you aren't asked for a password; start them from the tray menu or the Overview |
| Windows | scheduled task with highest privileges | one administrator prompt when you turn it on, none at login |
| macOS | LaunchAgent | none |

On Linux the tray needs a StatusNotifierItem host. GNOME needs the AppIndicator extension (`gnome-shell-extension-appindicator` on Arch). Without one, both switches are disabled and closing the window quits.

Turning a switch off removes what it installed: the autostart entry, the scheduled task or the LaunchAgent.

Only one copy of the app runs per account, on every platform. Launching it again asks the running copy to show its window and exits; on Windows that conversation goes over a named pipe only your account can open (see [the app's socket](command-line.md#the-apps-socket)). A login start that finds the app already running just exits.

## Permissions

The Live updates helper and start at login are **Permissions**: approvals beyond the app's core features. **Settings → Permissions** lists each one you granted and when; one granted before v0.3.0 says "Granted before this version". Its switch revokes it fully, the same as switching it off where it was granted: the helper stops and is not started again until you grant it again, and the login entry is removed. The list is kept in `settings.toml`.

Dismissing the helper's password prompt, or failing it, drops the helper's Permission: the Live updates switch turns back off and the app does not ask again at its next start.
