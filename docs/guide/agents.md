# Agents (MCP)

Your own agent (Claude, an IDE assistant, any MCP client) can ask the app about your files and your machine through `ve mcp serve`, an MCP server on stdin and stdout. It can only read: no tool changes, moves, deletes or stops anything. Each agent has to be approved once for each Module it reads.

## Connecting an agent

Point the agent's MCP configuration at the `ve` binary ([where to find it](command-line.md#where-to-find-it)) with the arguments `mcp serve`. For example, for Claude Desktop:

```json
{
  "mcpServers": {
    "view-everything": { "command": "/path/to/ve", "args": ["mcp", "serve"] }
  }
}
```

## Approving it

The agent's first call to a Module is refused: "approval pending, code 4F2K". The app shows a desktop notification naming the agent and the Module it asked for (Linux and macOS), and the request waits in **Settings → Permissions**. Approve it there, or from a shell:

```sh
ve mcp --list-requests     # CODE  MODULE  EXPIRES  AGENT
ve mcp --approve 4F2K
```

A code can be approved for 10 minutes. After that the agent gets a new one the next time it asks. Requests are held by the running app; `--list-requests` and `--approve` need it running.

An agent is its client name plus a key `ve mcp serve` issues it on first contact, kept in the data folder (`agent-keys.json`, readable by your account alone). A client that renames itself, or presents the same name with another key, is a new agent and needs its own approval; Settings says when approving it would replace the key an approved agent of that name holds. The key tells agents apart, not programs: any program running as you can read the key file, or run `ve mcp --approve` itself, so approve only the agents you pointed at `ve mcp serve`.

Approval writes an **Agent access** Permission: the agent, the Module and when you approved it. Approving one Module grants no other: an agent approved for Files is refused again, with a new code, when it first reads Performance. Switch the Permission off in Settings → Permissions to revoke it: the agent's next call to that Module is refused and asks for approval again.

## Tools

Every answer holds at most 200 rows (50 unless `limit` says otherwise) with a `total` over every row, and a `next_cursor` while more rows remain; pass it back as `cursor` for the next page. Each page is worked out again from what the app holds then, so a change between two calls can shift a row into the next page or out of it. Every answer says its `source` (`running app` or `saved Index file`).

### Files

All four take the [Query language](query-language.md), and each answer says its `last_synced`.

| Tool | Arguments | Answers |
| --- | --- | --- |
| `files.find` | `query` (required), `path`, `limit`, `cursor` | Entries the Query matches, by path: `total`, `files`, `folders`, `logical_size`, `on_disk_size`, and `rows` (`path`, `name`, `is_dir`, sizes, `modified_ns`, `category`, `owner`, `owner_kind`) |
| `files.top` | `path`, `query`, `files` (bool), `limit`, `cursor` | folders (or files) by On-disk Size, largest first; Filtered Size under a Query |
| `files.locations` | `limit`, `cursor` | each Location: `name`, `path`, `kind`, `fs_type`, `online`, `last_synced`, `files`, sizes, `capacity` |
| `files.summary` | `path`, `query`, `limit`, `cursor` | `files` and sizes under the path, and `rows` by Category, largest first |

### Performance

What the [Performance](performance.md) recorder kept of the last hour. Times are Unix seconds; `at` omitted is Live, the newest reading. A reading the recorder did not take (a gap, or a counter this system does not have) is `null`, never 0. Recording runs while the app's window is open or it keeps running in the tray, so the hour can have gaps.

| Tool | Arguments | Answers |
| --- | --- | --- |
| `performance.summary` | `from`, `to` (default: the last hour) | `seconds` recorded, mean `cpu` busy share and `memory` share used (0 to 1), and `top_process` (`pid`, `name`, `cpu`): the one that took the most CPU over the stretch |
| `performance.processes` | `at`, `sort` (`cpu` or `memory`), `limit`, `cursor` | the process table read at that second (`time`), busiest first: `pid`, `ppid`, `name`, `cpu` (0 to 1), `memory` (resident bytes), `io_read` and `io_write` (bytes per second), `started`, `cpu_seconds`. Live lists every process; a past second keeps the top 200 |

### Network

What the same recorder kept of the [Network](network.md): times, `at` and `null` work as for Performance. Per-process traffic is not read yet, so no tool answers it.

| Tool | Arguments | Answers |
| --- | --- | --- |
| `network.interfaces` | `from`, `to` (default: the last hour), `limit`, `cursor` | `seconds` with a network reading, `connections` (distinct connections those seconds kept, each the top 200), and `rows` by interface, busiest first: `name`, `received` and `sent` (bytes over the stretch), `last_seen`. An interface that went away keeps its row |
| `network.connections` | `at`, `limit`, `cursor` | the connections read at that second (`time`), open conversations first: `protocol`, `local`, `remote` (`null` while it has none), `state`, `holder` (`process`, `nobody` for one the kernel is closing, `unknown` for another user's process) with its `pid` and `process` name. Live lists every one; a past second keeps the top 200 |

### Apps

What the [Apps](apps.md) page lists, read by the running app.

| Tool | Arguments | Answers |
| --- | --- | --- |
| `apps.programs` | `name` (any letter case), `limit`, `cursor` | the installed programs, largest first by On-disk Size: `sized` (how many have a size) and their `on_disk_size`, and `rows`: `name`, `kind`, `source`, `id`, `version`, `installed`, `on_disk_size` (`null` when none of its folders is in the Index) and `roots` (each folder with its size) |
| `apps.startup` | `limit`, `cursor` | the Startup items: `name`, `kind` (`Autostart` or `systemd unit`), `id`, `command`, `source` (the file that defines it), `scope` (`User` or `System`), `only_in` and `not_in` (desktops), `drop_ins` |

A call that cannot be answered comes back as an error result whose `structuredContent.refusal` says why, so the agent can correct it:

| `reason` | When |
| --- | --- |
| `approval_pending` | no Permission yet; `code` and `expires_at` say what to approve |
| `not_approved` | no Permission, and the app is not running to ask you |
| `bad_query` | the Query does not parse (the message names the modifier) |
| `bad_path` | `path` is not absolute, or not in the Index |
| `bad_arguments` | an argument the tool does not take, `from` after `to`, or a cursor it did not hand out |
| `unknown_tool` | any other tool name |
| `no_sample` | nothing was recorded at that second: the recorder did not run then, or it is more than an hour ago |
| `loading` | the app is still reading its Index (Files, and `apps.programs`, whose sizes come from it) |
| `not_running` | the Module is read by the running app (Performance, Network, Apps), and it is not running |
| `no_index` | no app running and no saved Index yet |
| `failed` | not the agent's doing: the app's answer could not be read, the agent's key could not be saved, or the running app is an older version that does not answer this Module |

## With the app closed

`ve mcp serve` asks the running app, so answers include the last seconds of changes. With the app closed it reads the saved Index file for the Files tools, for an agent already approved in the saved settings. Reading the file, `files.locations` cannot say which Locations are online (`null`), `is:offline` is a bad Query, and `dupe:`, `is:duplicate` and `is:cleanable` find nothing. An agent that is not approved yet gets `not_approved`: start the app so it can ask you. The Performance, Network and Apps tools need the app running (`not_running`): its recorder keeps the hour in memory, not on disk, and the Apps page's list is the app's.
