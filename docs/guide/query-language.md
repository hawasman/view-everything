# Query language

Every panel in the Explorer (the tree, the flat list, the Treemap and Duplicates) follows one Query.
The Cleaner's [Cleanup Rules](cleanup-rules.md) and the [command line](command-line.md) use the same language.

## Basics

| Write | Matches |
| --- | --- |
| `report 2024` | names containing both terms (terms separated by spaces must all match) |
| `jpg\|png` | either term |
| `!tmp` | everything except names containing `tmp` |
| `"annual report"` | the phrase, spaces included |
| `IMG_*.jpg`, `file?.txt` | wildcards: `*` and `?` match against the whole name |
| `src/main` | a term containing `/` or `\` matches against the full path |

Text ignores case unless you use `case:`.

## Modifiers

### What a file is

| Modifier | Matches |
| --- | --- |
| `ext:mp4;mkv` | files with one of the extensions |
| `type:video` | files of a Category |
| `size:>1gb`, `size:1mb..5mb` | files by Logical Size |
| `kind:file`, `kind:folder` | files only, or folders only |
| `dupe:`, `is:duplicate` | files with a Duplicate (see [Duplicates](duplicates.md)) |
| `is:cleanable` | the Cleanables the [Cleaner](cleanup-rules.md) found in its latest look (the Overview and the Cleaner look); the files inside them count toward Filtered Size |
| `is:offline` | Entries on an Offline Volume (one not mounted now); Remotes are never offline |

### When it changed

| Modifier | Matches |
| --- | --- |
| `dm:2025-01`, `dm:>=yesterday` | files by modification date |
| `dc:2025-01`, `dc:<2020` | files by creation date, written like `dm:` |
| `since:7d`, `since:12h`, `since:2026-10-01` | files modified within the last minutes (`30m`), hours (`12h`), days (`7d`) or weeks (`2w`), or since the start of a date written like `dm:` |

`dc:` never matches a file whose creation time is unknown. That covers S3 Remotes, SFTP, FTP servers without the MLSD `create` fact, and filesystems without a birth time.

### Names

| Modifier | Matches |
| --- | --- |
| `regex:^IMG_\d+` | names matching a regular expression |
| `case:Report` | the term, matching letters in exactly that case |
| `wholeword:cat` | the term only as a whole word: `cat` in `a-cat.txt`, not in `category` |

For `wholeword:`, only letters and digits join a word, so `_`, `-`, `.` and spaces separate words. With wildcards (`wholeword:re*t`) the pattern must cover a whole run of words.

`case:` and `wholeword:` prefix a plain or wildcard term, chain (`case:wholeword:Cat`), and combine with `!` and `|` (`!wholeword:tmp`, `case:Foo|case:Bar`).

### Where it is

| Modifier | Matches |
| --- | --- |
| `parent:/home/me/Docs` | Entries directly inside that folder, not deeper; ignores case |
| `at:/home/me/Docs`, `at:C:\Users` | that folder and everything below it; ignores case. The path starts at a root (`/`, a drive, or a Remote's address) |
| `depth:2`, `depth:>1`, `depth:1..3` | Entries by levels below the Location root (the root is 0) |
| `child:.git`, `child:*.toml` | folders with a direct child whose name matches |
| `empty:` | folders with no children |
| `owner:steam`, `owner:"Program Files"` | Entries under an Owner whose name contains the text, folders as well as files; ignores case |
| `ownerkind:game`, `ownerkind:none` | Entries under an Owner of that kind (`system`, `program`, `game`, `project`), or under none (see [Owners](owners.md)) |

Facets combine with text and each other like any term: `report kind:file since:7d !at:/home/me/old`, `is:cleanable|is:duplicate`.

## When a Query is wrong

A Query that does not parse says why in the Query box, naming the modifier, such as ``since: `soon` is not a duration or date`` or ``kind: unknown value `dir`; expected file or folder``. The panels keep showing the last Query that worked until it is fixed.

## In the app

- Double-click a folder anywhere to zoom into it. The zoom becomes part of the Query, so you can keep refining it.
- The Query runs after a short pause in typing.
- The ✕ in the Query box clears it. Escape first drops the search terms and keeps the folder you zoomed into, then clears the rest.
- The Filters panel builds clauses for you; its Reset removes only the clauses it added.
- Saved Queries keep a Query you use often.
