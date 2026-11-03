# Apps

The **Apps** page lists the programs installed on this computer and what each takes on disk, and, on its **Startup** tab, what starts when you log in. It appears in the rail, under Files, once at least one installed program or startup item is known. For now it reads Linux only, and changes nothing: there is no uninstall, and no switch to turn a startup item off.

## What is listed

| Kind | Read from |
| --- | --- |
| Package | the pacman database (`/var/lib/pacman/local`), the dpkg database (`/var/lib/dpkg/status`) and the rpm database (through `rpm -qa`) |
| Flatpak | applications installed system-wide (`/var/lib/flatpak/app`) and for you (`~/.local/share/flatpak/app`) |
| Game | Steam games in every library your Steam knows (its own folder and the ones `libraryfolders.vdf` lists) whose install folder is there |
| Program | each folder in `/opt` |

Each row shows the name, kind, version, install date and **On disk** size. **Installed** is when the installed version arrived: the package database's install time (dpkg: when the package's file list was written), when a Flatpak's current deployment was made, Steam's last update, or when the `/opt` folder was created. A value the source does not give shows "—".

Click a column title to sort by name, kind, install date or size; click it again to reverse. Programs without a date or size stay at the bottom either way.

## Sizes

A program's size is the Folder Size of its [Owner roots](owners.md) in the Index: the folders Owner detection gave that program. It is what Browse shows for those folders, and a hard-linked file counts once.

A program none of whose folders is in the Index shows "—" rather than a guess. That is the case for a program on a Volume you do not index, for most packages (a package's Owner roots are only the folders it owns wholly, such as `/usr/share/doc/<package>`, not its files in `/usr/bin`), and for every rpm package, whose file lists are not read yet.

The list is read again after each Sync, once Owner detection has finished, so a program installed since appears with its size.

## Inspecting a program

Click a row to open the program in the Inspector: its source and id, version, install date, size and each Owner root with its size. **Show in Browse** opens its largest Owner root in Browse.

## Startup

The **Startup** tab lists what starts when you log in, each with what it runs and the file that defines it:

| Kind | Read from |
| --- | --- |
| Autostart | XDG autostart entries: `autostart/*.desktop` in your configuration folder (`~/.config`, or `$XDG_CONFIG_HOME`), then in the system ones (`/etc/xdg`, or `$XDG_CONFIG_DIRS`) |
| systemd unit | systemd user units enabled in the default target: every name in a `default.target.wants` folder on the user unit search path (`~/.config/systemd/user`, `/etc/systemd/user`, `~/.local/share/systemd/user`, `/usr/lib/systemd/user` and the rest) |

An autostart entry of yours hides the system entry with the same file name, as your desktop does: the tab shows yours. An entry with `Hidden=true` or `X-GNOME-Autostart-enabled=false` starts nothing and is left out, and hides the system one of the same name. **Runs** is its `Exec=` line, without the `%U`-style codes a launcher fills in with files.

A systemd unit is read the way systemd loads it: by name from the first folder on the search path that has it, so your own copy in `~/.config/systemd/user` hides the system's; an instance (`name@instance.service`) falls back to its template. Its drop-ins (`<unit>.d/*.conf`) are applied, so **Runs** is the `ExecStart=` it really runs. A masked unit (linked to `/dev/null`) or an enabled one no folder has does not start and is not listed. A timer, socket or path unit runs nothing itself and shows "—".

Click a row to open the item in the Inspector: what it runs and the file that defines it in full, its drop-ins, whether it is yours or the system's, and the desktops an autostart entry is limited to.
