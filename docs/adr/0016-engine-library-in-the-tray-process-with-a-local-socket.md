# The engine is a library in the tray process, reached over a local socket

The Launcher, Performance and Network recording, and agent access (MCP) all need something running while the window is closed, and the CLI and MCP server need something to talk to. Today the engine (`Index`, Tasks, Locations, Live updates) is a set of threads inside `ve-app`, and `ve` reads `index.bin` directly.

The engine moves into its own crate, `ve-engine`, with no GUI dependency: Index, Tasks, Locations, Live updates and the Timeline recorders. The tray process hosts it and listens on a local socket (`$XDG_RUNTIME_DIR/view-everything/engine.sock`; a named pipe on Windows) speaking newline-delimited JSON, one request per line, streamed replies as repeated lines. The GUI, the tray, the Launcher window, `ve` and the MCP server are clients of that API. `ve` falls back to reading `index.bin` when nothing is listening.

The process is still the tray app. ADR-0003 and ADR-0012 hold: no privileged service, no daemon, nothing installed beyond the app. A separate (possibly privileged) engine process is still a later decision, now reduced to moving the host of an existing API; the trigger stays Network background recording.

Alternatives: a daemon binary now (per-platform installers, Index file ownership and elevation design before any feature needs them); gRPC (brings tokio and prost into a workspace that has neither; the engine is thread-based); MCP inside the GUI process with no socket (the Launcher and the CLI would still need one).

## Consequences

- `ve-app` becomes a client plus the host: it starts the engine, owns the socket, and renders. Pages read through the same API the CLI uses, so an aggregate the GUI can show, an agent can ask for.
- Only one process may host the engine per user; a second launch connects to the first (the single-instance rule the tray already needs).
- The API is versioned from the first line; a client with the wrong version is refused with a reason.
- Timeline recording runs only while the engine runs: with the window open, or with keep-in-tray on. Gaps are recorded as gaps.
