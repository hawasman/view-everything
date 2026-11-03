# Agents (MCP)

Your own agent (Claude, an IDE assistant, any MCP client) can ask the app about your files through `ve mcp serve`, an MCP server on stdin and stdout. It can only read: no tool changes, moves or deletes anything. Each agent has to be approved once for the Module it reads.

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

The agent's first call is refused: "approval pending, code 4F2K". The app shows a desktop notification naming the agent and the Module it asked for (Linux and macOS), and the request waits in **Settings → Permissions**. Approve it there, or from a shell:

```sh
ve mcp --list-requests     # CODE  MODULE  EXPIRES  AGENT
ve mcp --approve 4F2K
```

A code can be approved for 10 minutes. After that the agent gets a new one the next time it asks. Requests are held by the running app; `--list-requests` and `--approve` need it running.

An agent is its client name plus a key `ve mcp serve` issues it on first contact, kept in the data folder (`agent-keys.json`, readable by your account alone). A client that renames itself, or presents the same name with another key, is a new agent and needs its own approval; Settings says when approving it would replace the key an approved agent of that name holds. The key tells agents apart, not programs: any program running as you can read the key file, or run `ve mcp --approve` itself, so approve only the agents you pointed at `ve mcp serve`.

Approval writes an **Agent access** Permission: the agent, the Module and when you approved it. Switch it off in Settings → Permissions to revoke it: the agent's next call is refused and asks for approval again.

## Tools

All four are read-only and take the [Query language](query-language.md). Each answer holds at most 200 rows (50 unless `limit` says otherwise) with totals over every row, and a `next_cursor` while more rows remain; pass it back as `cursor` for the next page. Each page is worked out again from the Index as it is then, so a change between two calls can shift a row into the next page or out of it. Every answer says its `last_synced` and its `source` (`running app` or `saved Index file`).

| Tool | Arguments | Answers |
| --- | --- | --- |
| `files.find` | `query` (required), `path`, `limit`, `cursor` | Entries the Query matches, by path: `total`, `files`, `folders`, `logical_size`, `on_disk_size`, and `rows` (`path`, `name`, `is_dir`, sizes, `modified_ns`, `category`, `owner`, `owner_kind`) |
| `files.top` | `path`, `query`, `files` (bool), `limit`, `cursor` | folders (or files) by On-disk Size, largest first; Filtered Size under a Query |
| `files.locations` | `limit`, `cursor` | each Location: `name`, `path`, `kind`, `fs_type`, `online`, `last_synced`, `files`, sizes, `capacity` |
| `files.summary` | `path`, `query`, `limit`, `cursor` | `files` and sizes under the path, and `rows` by Category, largest first |

A call that cannot be answered comes back as an error result whose `structuredContent.refusal` says why, so the agent can correct it:

| `reason` | When |
| --- | --- |
| `approval_pending` | no Permission yet; `code` and `expires_at` say what to approve |
| `not_approved` | no Permission, and the app is not running to ask you |
| `bad_query` | the Query does not parse (the message names the modifier) |
| `bad_path` | `path` is not absolute, or not in the Index |
| `bad_arguments` | an argument the tool does not take, or a cursor it did not hand out |
| `unknown_tool` | any other tool name |
| `loading` | the app is still reading its Index |
| `no_index` | no app running and no saved Index yet |
| `failed` | not the agent's doing: the app's answer could not be read, or the agent's key could not be saved |

## With the app closed

`ve mcp serve` asks the running app, so answers include the last seconds of changes. With the app closed it reads the saved Index file, for an agent already approved in the saved settings. Reading the file, `files.locations` cannot say which Locations are online (`null`), `is:offline` is a bad Query, and `dupe:`, `is:duplicate` and `is:cleanable` find nothing. An agent that is not approved yet gets `not_approved`: start the app so it can ask you.
