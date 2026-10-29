# Staying current: Sync, Live updates and Background

The Index is saved between runs, so search and sizes never wait on a scan. Four mechanisms keep it up to date.

| Mechanism | When it runs |
| --- | --- |
| **Catch-up** | at launch, for Locations whose Last Synced is stale |
| **Periodic Sync** | on each Location's schedule: every 30 minutes for local Volumes, 120 for network shares, Manual for Remotes (all adjustable in Settings) |
| **Sync now** | when you ask: for every Volume (Dashboard, tray), one Location (Dashboard, Settings) or one folder (**Sync this folder** in the Explorer's right-click menu and the Selection panel) |
| **Live updates** | continuously while the app runs, where the platform allows it |

Sync now re-reads in full, even a Volume Live updates is following. Every Sync shows up as a Task and can be cancelled. A cancelled Sync leaves the Location's earlier Entries in place.

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
| Linux | XDG autostart entry | none. Live updates stay paused after a login start, so you aren't asked for a password; start them from the tray menu or the Dashboard |
| Windows | scheduled task with highest privileges | one administrator prompt when you turn it on, none at login |
| macOS | LaunchAgent | none |

On Linux the tray needs a StatusNotifierItem host. GNOME needs the AppIndicator extension (`gnome-shell-extension-appindicator` on Arch). Without one, both switches are disabled and closing the window quits.

Turning a switch off removes what it installed: the autostart entry, the scheduled task or the LaunchAgent.
