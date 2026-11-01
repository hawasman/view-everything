# Command line

`ve` asks the running app, so its answers include changes from the last few seconds. With the app closed it reads the saved Index instead, and says so on stderr. It never writes the Index. Open the app once to build it.

## Where to find it

| Platform | Location |
| --- | --- |
| Linux (AppImage) | `usr/bin` inside the AppImage; get it out with `./view-everything-x86_64.AppImage --appimage-extract` |
| Linux (AUR) | on your `PATH` |
| Windows | next to `view-everything.exe` |
| macOS | `View Everything.app/Contents/MacOS/` |

## Commands

```
ve find   [--csv|--json] [--path DIR] <QUERY>...                     matching paths, one per line
ve top    [--csv|--json] [--path DIR] [--files] [-n N] [QUERY]...    largest folders (or files) by On-disk Size
ve export [--json] [--path DIR] [QUERY]...                           CSV of a Query's results (everything without one)
```

- `ve du` is the same as `ve top`. Under a Query it ranks by Filtered Size.
- `top` prints `<bytes>\t<path>`.
- `--index FILE` reads that saved Index file instead of asking the app.

The Query is the same [language](query-language.md) as the app's. Quote it for your shell, or put `--` before words that start with `-`.

## Examples

```sh
ve find ext:iso size:>1gb          # large disk images
ve top -n 20 --path ~/Projects     # the 20 largest folders under Projects
ve top --files type:video          # the largest videos
ve export dm:>=2025-01 > recent.csv
```

## Output and exit codes

Last Synced goes to stderr, so pipes stay clean. So does `ve: the app is not running; reading the saved Index file` when `ve` falls back to the file.

| Exit code | Meaning |
| --- | --- |
| 0 | something was printed |
| 1 | no match |
| 2 | a bad argument or Query, or an unreadable Index |

## The app's socket

While it runs, the app listens on `$XDG_RUNTIME_DIR/view-everything/engine.sock` (a per-user folder in the temporary folder where there is no `XDG_RUNTIME_DIR`; the `VE_SOCKET` variable overrides it). The folder is readable by your account only, so other users on the machine cannot read your Index through it. Windows has no socket yet; there `ve` always reads the file.

Asking the app, `dupe:` and `is:duplicate` use the Duplicates it has found so far, `is:cleanable` what the Cleaner last found, and `is:offline` the Volumes mounted now. Reading the file, `dupe:`, `is:duplicate` and `is:cleanable` find nothing, and `is:offline` is a bad Query: the file does not know which Volumes are mounted.

Launching the app a second time uses the same socket: the running app shows its window and the new launch exits.

### Protocol

Newline-delimited JSON, one message per line. The client opens with `{"version": 1}`; the app answers `{"version": 1, "engine": "view-everything 0.3.0"}`, or refuses another version with `{"error": "...", "expected": 1}` and closes. Then each request is `{"id": 1, "method": "...", "params": ...}`, and every reply carries its `id`: one `{"id", "result"}`, a stream of `{"id", "item"}` lines ending with `{"id", "end"}`, or `{"id", "error"}`.

| Method | Params | Replies |
| --- | --- | --- |
| `echo` | anything | `result`: the params |
| `show` | none | shows the window; `result`: `null` |
| `search` | `query`, `path`, `select` (`matches`, `largest_folders`, `largest_files`), `limit` | one `item` per Entry (`path`, `name`, `is_dir`, `logical_size`, `on_disk_size`, `modified_ns`, `category`, `owner`, `owner_kind`); `end`: `count`, `last_synced` |
| `tasks` | `follow` (bool) | one `item` per running Task (`task`, `kind`, `label`, `done`, `total`, `unit`); following, then one per change, and one with `outcome` when a Task finishes, until none runs; `end`: `{}` |

```sh
printf '{"version":1}\n{"id":1,"method":"search","params":{"query":"ext:iso","limit":3}}\n' \
  | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/view-everything/engine.sock
```
