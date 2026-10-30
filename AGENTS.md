# Agent instructions

## Testing the UI

Never drive the user's real desktop: no `xdotool`, `ydotool`, `wtype` or screenshots against the live session (`DISPLAY=:0`, the user's `WAYLAND_DISPLAY`). The user is working on that machine, and real input locks them out until you finish.

Use `scripts/ui-sandbox.sh` instead. It runs the app on a hidden KWin desktop with its own input, display, D-Bus session and data folders.

```bash
cargo build -p ve-app --release                  # the script runs the binary; it does not build
scripts/ui-sandbox.sh start NAME                 # NAME: unique per agent, e.g. your task name
scripts/ui-sandbox.sh shot  NAME /tmp/NAME/1.png
scripts/ui-sandbox.sh click NAME X Y             # X,Y are pixels of the screenshot
scripts/ui-sandbox.sh key   NAME ctrl+f
scripts/ui-sandbox.sh type  NAME "query"
scripts/ui-sandbox.sh xdo   NAME <xdotool args>  # anything else xdotool does
scripts/ui-sandbox.sh stop  NAME                 # always stop when done
```

- Pick a NAME no other agent is likely to use; `scripts/ui-sandbox.sh list` shows running sandboxes. Never stop one you did not start.
- Screenshots cover the whole sandbox desktop at 1:1, so screenshot pixels are click coordinates. `start NAME 1920x1200` changes the size; the window opens at 1440x900.
- Each sandbox starts with an empty Index and syncs it on launch. Wait for the sync, or re-screenshot, before judging numbers.
- After each action, take a screenshot and check the result before the next step. If a click changed nothing, re-check the coordinates before retrying.
- The app exposes no usable accessibility tree yet, so target elements by screenshot coordinates.
- Rebuild, then `stop` and `start` again, to test a code change; a running sandbox keeps the old binary.
- Logs: `$XDG_RUNTIME_DIR/ve-ui-sandbox/NAME/app.log` (app) and `kwin.log`.
- `VE_BIN` may point at a wrapper script, which is how to put a tool around the app. Frame times and GPU/CPU load: `scripts/ui-sandbox.sh perf NAME` starts a sandbox under MangoHud (overlay top-right, frame-time CSV logs in `$XDG_RUNTIME_DIR/ve-ui-sandbox/NAME/mangohud/`). Heap: a wrapper that sets `LD_PRELOAD=/usr/lib/libjemalloc.so.2 MALLOC_CONF=prof:true,prof_active:true,lg_prof_sample:18,lg_prof_interval:29,prof_prefix:/tmp/jeprof/ve`, then `jeprof --text target/release/view-everything /tmp/jeprof/ve.*.heap`.
- Slow frames: `VE_FRAME_TRACE=8` (threshold in ms) makes every `render` slower than that print `frame-trace: <view> took N ms` to `app.log`; a wrapper script as `VE_BIN` sets it. A long `Shell::render` is the frame waiting for the Index lock (a Sync is writing); a long page render is that page's own work.

## Domain language

`CONTEXT.md` defines the project's terms (Index, Entry, Location, Module, ...). Use them in code, UI text and messages; architectural decisions are in `docs/adr/`.
