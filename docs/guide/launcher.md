# Launcher

The **Launcher** is a small window that finds files and folders in the Index, and installed programs, by name, from any app, without the main window. Press its shortcut (**Alt+Space** unless you changed it), type part of a name, and press Enter.

| Key | Does |
| --- | --- |
| typing | lists the Entries and programs whose names hold the text |
| Up, Down | move the selection |
| Enter | opens the selected Entry with its default app (a folder in the file manager), or starts the selected program |
| Shift+Enter | shows an Entry in **Browse** in the main window, opening the window if it is closed |
| Esc | closes the Launcher; so does clicking another window |

The Launcher is keyboard only. It remembers what you typed until the app quits, so reopening it continues where you left off. It never asks for anything and sends nothing over the network.

## What it lists

At most 50 Entries and programs together, best first. A name that starts with the text comes first, then one where a word starts with it (after a space, `-`, `_`, `.` or the like, or at a capital after a small letter, as in `MyReport`), then one that holds it anywhere. Within each group programs come before Entries (so an application is not lost among its own icons and documentation), then the most recent first: a program by when it was installed, an Entry by when it was modified. Case does not matter.

Each row shows the name, the folder it is in and whether it is a File or a Folder. An Entry on an **Offline** Volume is marked so and cannot be opened (its Volume is not mounted); one on a **Remote** is marked too and cannot be opened from this computer. Shift+Enter shows either in Browse.

## Programs

The programs are those **Apps** lists that the Launcher knows how to start:

| Marked | What | Enter runs |
| --- | --- | --- |
| **Program** | a pacman or dpkg package that installed a desktop entry a menu shows (row: the desktop entry's file) | that desktop entry's command |
| **Flatpak** | a Flatpak application (row: its id) | `flatpak run <id>` |
| **Game** | a Steam game (row: its app id) | `steam://rungameid/<id>`, which Steam opens |

Others are not listed, since no command to start them is known: packages without such a desktop entry (libraries, and tools that run in a terminal), rpm packages, and folders in `/opt`. A program starts on its own: it keeps running when the app quits.

## With the main window closed

The Launcher belongs to the app's tray process, not to the main window. With **Settings → Background → Closing the window keeps the app in the tray** on, the shortcut and the tray menu's **Launcher** item keep working after you close the window. With it off, closing the window quits the app, and the shortcut with it.

## Changing the shortcut

**Settings → Launcher → Shortcut** takes modifiers and one key, such as `Alt+Space`, `Ctrl+Shift+K` or `F12`; press Enter to apply it. The line beneath says what came of it:

- **Alt+Space opens the Launcher from any app**: the shortcut is registered.
- **Your desktop bound … to the Launcher**: on Wayland the desktop decides through its global shortcuts portal and may choose another key, or keep the one it bound before; change it in the desktop's shortcut settings.
- **… is already taken by another program**, or another reason the desktop refused: the Launcher still opens from the tray menu, and you can bind the command `view-everything --launcher` to a key in your desktop's own shortcut settings.

On X11, Windows and macOS the app takes the key itself. On Wayland it asks the desktop through the GlobalShortcuts portal, which KDE Plasma offers; a desktop without it refuses. The portal knows the app by its id, `io.github.hawasman.view-everything`, and accepts it only when it finds the desktop file of that name, which the packages install; a build run without it (from source, or an AppImage your desktop integrated under another name) is refused with "An app id is required".

## `view-everything --launcher`

Running `view-everything --launcher` opens the running app's Launcher (or closes it when it is open) and exits. If the app is not running, it opens a Launcher that says so, since there is no Index to search; it closes with Esc.
