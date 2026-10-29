# Cleaner and Cleanup Rules

The Cleaner lists what can be regenerated or discarded, with each item's On-disk Size. Each item it lists is a Cleanable. It finds them with Cleanup Rules: built-in ones, plus [your own](#write-your-own-rules).

## What it finds

| Group | Examples | Pre-ticked |
| --- | --- | --- |
| Temporary Files | the user temp folder | when nothing in it changed for 7 days |
| Caches | the user cache folder; on Windows the Internet cache and shader cache | never |
| Build Artefacts | Rust `target/`, Next.js `.next`, `node_modules`, other tool output | when nothing in the project changed for 30 days (`node_modules` never) |
| Admin only | items you don't own, or that hold something you don't own (`/var/cache/pacman`, items under `C:\Windows`) | never |

- Both age limits are in Settings.
- A build artefact counts only when a project marker is found, such as a sibling `Cargo.toml` or `package.json`.
- Remotes, Offline Volumes and read-only mounts are never offered.

## Using the Cleaner

1. Tick items to form a **Cleanup Plan**. Ticks stay when you change page or tab. Each group tab shows 25, 50 or 100 items at a time.
2. Open **Advanced** to review the exact paths and commands.
3. Press **Clean**.

Clean removes items **permanently**, not via the Trash ([ADR-0008](../adr/0008-clean-removes-permanently.md)). Before removing anything, it checks each item again on disk:
- it is still the same kind of folder;
- the marker is still present;
- there is no link on the way to it;
- nothing in it changed within the age limit, if it was ticked for being old.

There is no Retry for a Clean, on purpose. Run the Cleaner again to see the new state.

### Rules that run a command

Some rules remove by running a tool's own command, such as `npm cache clean --force`.
- The command runs without a shell.
- Its size is only an Estimated one.
- Its output and exit status are shown after Clean.

### Admin only

Items under **Admin only** show the exact command that would remove them; the normal Clean never touches them.

| Platform | Clean as admin |
| --- | --- |
| Linux | shows the command and runs it with `pkexec` (one password prompt), then reports per item whether it is gone |
| macOS, Windows | the app does not elevate: run the shown command yourself |

Command rules are never run with admin rights.

### Elsewhere in the app

- The Category breakdown shows how much of the reclaimable space each Category takes, for every Cleanable or for the ticked Cleanup Plan.
- **Show in Explorer** on a Cleanable sets the Query to its folder.
- The Dashboard's Cleaner card shows the total the Cleaner could reclaim and opens the Cleaner.

## Write your own rules

Rules are written in TOML. The built-in ones ([`builtin.toml`](../../crates/ve-app/src/cleaner/builtin.toml)) use the same format as yours. Yours go in `cleanup.toml` next to `settings.toml`:

| Platform | Folder |
| --- | --- |
| Linux | `~/.config/view-everything/` |
| macOS | `~/Library/Application Support/view-everything/` |
| Windows | `%APPDATA%\view-everything\` |

The file is read again whenever it changes. A mistake shows in the Cleaner with the file, line and reason, and the built-in rules keep working. A syntax error skips the whole file; a bad rule skips only itself.

```toml
[[rule]]
name = "Python virtualenvs"      # shown in the Cleaner
group = "build"                  # temp, cache or build
query = 'regex:^\.venv$'         # a Query: what the rule matches
markers = ["../pyproject.toml", "../requirements.txt"]   # one must be a plain file
tick = false                     # never pre-ticked

[[rule]]
name = "cargo clean"
group = "build"
query = 'child:Cargo.toml'       # folders holding a Cargo.toml
markers = ["Cargo.toml"]
command = ["cargo", "clean"]     # run in the matched folder, without a shell
estimate = "target"              # what to size for the Estimated figure
```

### Keys

| Key | |
| --- | --- |
| `name`, `group` | Required. `group` is `temp` (Temporary Files), `cache` (Caches) or `build` (Build Artefacts). |
| `query` | Required. A [Query](query-language.md) over the Index. `{home}`, `{cache}` and `{temp}` stand for those folders of the user, so `parent:"{cache}"` selects what is directly inside the user cache folder. |
| `markers` | Required unless the Query names a location with `parent:`. Paths relative to a match, such as `../package.json` (a sibling) or `CACHEDIR.TAG` (inside). A match counts only when one is a plain file, never a link. |
| `files` | `true` lets files match too, not only folders. |
| `tick` | `false` never pre-ticks the rule. Caches are never pre-ticked either. |
| `platforms` | Any of `linux`, `macos`, `windows`; every platform when left out. |
| `command` | Removes by running this argv, without a shell, in the matched folder, instead of removing the matched paths. The Cleaner shows the exact argv and folder, marks the size Estimated, reports output and a non-zero exit after Clean, then reads the folder's Volume into the Index again. |
| `estimate` | With `command`: a path inside the matched folder whose size is shown. |

Clean checks every match again on disk (the Query, the markers, no links, no Volume mounted inside) before it removes or runs anything.
