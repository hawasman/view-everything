# view-everything — Product Requirements

Status: living document. Terms follow `CONTEXT.md`; decisions are in `docs/adr/`.

## 1. Vision

One free, open-source app to see and manage everything about your device: its files, apps, performance and network, with a global search. It brings what Everything, WizTree, Task Manager / btop, GlassWire and PowerToys Run each do into one fast, native interface that is easier to read than any of them.

## 2. Principles

1. **Trust.** The app never does anything the user did not approve. Installing it approves its core features and nothing more. Every privilege is explained — what it is, why it is needed, what the user gets — before it is asked for, at the moment it is needed, and every approval can be reviewed and revoked.
2. **Honesty.** Unsupported, unknown and zero are different states. A sample that was not taken leaves a gap; the app never shows a value it did not measure.
3. **Speed.** Native code, no web view. Interactions stay inside the budgets in §7.
4. **Privacy.** No telemetry, no accounts, no cloud. The only routine network request is the update check, and it can be turned off.
5. **Changes are reviewed.** Anything that removes or alters data on the device goes through a reviewed plan (Cleanup Plan, Trash confirmation), whether a person or an agent starts it.

## 3. Users

- **Primary:** power users and developers on their own machines (build artefacts, package caches, containers, toolchains).
- **Secondary:** gamers with large drives and performance questions ("why did the game stutter?").
- **Served by defaults, not targeted:** everyday users who want to know what fills their disk or slows their PC.
- **Out of scope:** IT administrators managing fleets.

## 4. Platforms

- **Files** works the same on Windows, Linux and macOS.
- New Modules start on Windows and Linux. macOS gets the parts its APIs allow without special entitlements.
- Each Module documents what it supports per platform; an unsupported reading is shown as unsupported, never as zero.
- Headless use on a single Linux server is supported through the CLI and MCP (§6.1).

## 5. Modules and roadmap

A **Module** is a top-level area of the app with its own pages. Modules ship in this order.

### 5.1 Files — shipped (v0.1, v0.2), continuing

Shipped:
- One shared Index of every Entry on local Volumes and Remotes (SSH/SFTP, S3, FTP, WebDAV, Azure Blob); Live updates (FSEvents, opt-in `fanotify` helper), Catch-up, Sync now per Location and per folder.
- Search with the Query language; Saved Queries; Explorer tree and flat list with multi-select and context menu; Treemap with Depth, header strips and Selection panel; Category breakdown; CSV and PNG export.
- Cleaner with Cleanup Rules, Cleanup Plan, Clean and admin-only steps; Trash and Empty Trash.
- Duplicates with staged BLAKE3 hashing and Duplicate Exclusions.
- Daily Snapshots and Compare with.
- Tasks with progress and cancel; tray and start at login; signed self-updates.
- CLI: `ve find`, `ve top`/`du`, `ve export`.

Next (v0.3):
- **Owner** detection: the program, game, package or project a folder belongs to (Steam manifests, `Program Files\<Vendor>`, package databases, Flatpak ids, project markers), shown in tooltips and the Selection panel.
- Treemap "Colour by: Category | Owner kind".

### 5.2 Search

- A global hotkey opens a launcher over the Index, installed apps and app actions (in the manner of PowerToys Run).
- Results within the keystroke budget (§7); keyboard-only operation.
- The reason to keep the app in the tray.

### 5.3 Performance

Answers *why* the machine is slow, not only *what* is running: btop-style live views, plus a recorded history to go back to when a problem started.
- CPU (performance and efficiency cores distinct), memory pressure and composition, disk, GPU, thermals, energy.
- Processes tracked by identity (PID plus creation identity) so a recycled PID never redirects an action.
- A **Timeline** recorded while the app runs: per-second samples for the last hour, per-minute samples for 30 days, capped at 200 MB (configurable). Users scrub back to when a problem started and see which process and subsystem moved first.
- Gaps where sampling stopped; unsupported sensors shown as unsupported.

### 5.4 Apps

- Installed programs with their real On-disk Size (linked to their Owner folders), uninstall, and leftovers after uninstall.
- Startup items, with the same explain-then-approve rule for changes.

### 5.5 Network

- Without privileges: per-interface throughput, totals and connection lists.
- Per-app traffic needs elevation on most platforms. The page shows what it can and offers one button, "Show traffic per app (needs administrator)", with an explanation; nothing prompts when the page opens.
- Per-app history in the Timeline. "Keep recording in the background" is the feature that introduces the privileged service (§9), offered with an explanation once per-app traffic is approved.

### 5.6 Agent access (MCP and CLI)

- An MCP server and the CLI expose the same core operations.
- **Read** operations return aggregates by default (ranked folders, top processes, Timeline summaries) with row caps and a continuation cursor, so answers stay small.
- **Changes** are never applied directly: an agent can only propose a plan (Cleanup Plan, Trash list), which a person approves in the UI or by an explicit CLI confirmation.
- **Approval:** an agent sees nothing until the user approves it, per Module. An unapproved agent receives a clear refusal saying approval is pending; the user receives a notification (desktop or tray) to approve. Headless:
  - `ve-cli mcp --list-requests`
  - `ve-cli mcp --approve <CODE>`
- No model ships in the app; users bring their own agent.
- A read-only Files slice can ship early (around Search), since the Query and `top` already exist.

### 5.7 Plugins

- Sandboxed WASM plugins that add search sources, Cleanup Rules and Owner detectors.
- Plugins follow the trust principle: each declares what it reads and needs approval.
- Plugins that add whole Modules are deferred until the built-in Modules settle.

## 6. Cross-cutting capabilities

### 6.1 Headless core

The core (Index, Timeline recording, Tasks) runs without a GUI. The GUI, tray, CLI and MCP server are clients of it. On a server it runs alone. When it moves into its own process is decided by its own ADR, at the latest when Network background recording arrives.

### 6.2 Onboarding

- A short first run: what the app is, what it reads, that it sends nothing, and a one-line mention of each optional privileged feature.
- Each privileged feature explains itself the first time the user reaches it: what it is, why it needs the privilege, how it helps.

### 6.3 Permissions

- Settings → Permissions lists every approval the user has granted: elevated helpers, the background service, start at login, MCP agents and their Modules, plugins.
- Each shows when it was granted and has one switch to revoke it. Revoking undoes it fully (uninstall the service, remove the scheduled task or autostart entry, disconnect the agent).

### 6.4 Timeline and Snapshot

- **Timeline:** recorded time series (Performance, Network), browsable by time.
- **Snapshot:** the Folder Size of every folder at one moment (Files).
- Both can be compared at a moment ("what changed at 14:02").

## 7. Success metrics

Performance budgets, measured with `VE_BENCH` on release builds:

| Interaction | Budget |
|---|---|
| Query results after a keystroke, 10M Entries | ≤ 100 ms |
| Treemap hover | ≤ 16 ms |
| Treemap zoom | ≤ 150 ms |
| Page switch | ≤ 50 ms |
| Idle tray app | < 1% CPU, < 200 MB RAM |
| Cold start to a usable window | < 2 s |

No adoption targets.

## 8. Non-goals

- Cloud sync, accounts, telemetry.
- Managing several machines.
- Antivirus, registry cleaning, "PC booster" tricks.
- A paid tier: the app stays free and open source (MIT or Apache-2.0); donations only.
- A built-in AI model.

## 9. Open decisions (future ADRs)

- When the core moves into its own (privileged) process, and how clients talk to it.
- Plugin ABI, sandbox and permission model.
- Timeline storage format and retention.
- MCP protocol surface and the agent approval mechanism.
