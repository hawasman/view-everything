# Command line

`ve` reads the saved Index. It never writes it, so it works while the app runs. Open the app once to build the Index.

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
- `--index FILE` reads another saved Index.

The Query is the same [language](query-language.md) as the app's. Quote it for your shell, or put `--` before words that start with `-`.

## Examples

```sh
ve find ext:iso size:>1gb          # large disk images
ve top -n 20 --path ~/Projects     # the 20 largest folders under Projects
ve top --files type:video          # the largest videos
ve export dm:>=2025-01 > recent.csv
```

## Output and exit codes

The Index's Last Synced time goes to stderr, so pipes stay clean.

| Exit code | Meaning |
| --- | --- |
| 0 | something was printed |
| 1 | no match |
| 2 | a bad argument or Query, or an unreadable Index |

`dupe:` finds nothing from the command line, because it does not read `hashes.bin`.
