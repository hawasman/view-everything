#!/usr/bin/env bash
# Runs view-everything on a hidden KWin desktop so agents can drive and screenshot the UI
# without touching the user's screen, mouse, keyboard or focus.
#
#   scripts/ui-sandbox.sh start [NAME] [WIDTHxHEIGHT]  start a sandbox (default NAME=default, 1600x1000)
#   scripts/ui-sandbox.sh click NAME X Y [BUTTON]      move, settle, click at screen pixel X,Y
#   scripts/ui-sandbox.sh key   NAME KEYS...           xdotool key, e.g. ctrl+f Escape
#   scripts/ui-sandbox.sh type  NAME TEXT              type literal text
#   scripts/ui-sandbox.sh xdo   NAME ARGS...           any other xdotool command
#   scripts/ui-sandbox.sh shot  NAME FILE.png          full-desktop screenshot (pixels = click coordinates)
#   scripts/ui-sandbox.sh stop  NAME                   stop the sandbox and delete its data
#   scripts/ui-sandbox.sh list                         list running sandboxes
#
# Each sandbox has its own D-Bus session, KWin (--virtual), Xwayland display and XDG data
# folders, so several can run at once and none shares the user's Index. A fresh data folder
# means the app builds a new Index on start. VE_BIN overrides the binary
# (default: target/release/view-everything, falling back to target/debug).
#
# Each sandbox runs in its own systemd scope (ve-ui-sandbox-NAME.scope) with a memory cap, so a
# runaway sandbox is throttled, then killed whole, instead of the kernel killing the user's apps.
# VE_SANDBOX_MEMORY_HIGH (default 1536M), VE_SANDBOX_MEMORY_MAX (2G) and
# VE_SANDBOX_SWAP_MAX (1G) override the caps.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BASE=${XDG_RUNTIME_DIR:-/tmp}/ve-ui-sandbox

die() { echo "ui-sandbox: $*" >&2; exit 1; }
need() { command -v "$1" >/dev/null || die "missing $1"; }
dir_of() { local d=$BASE/${1:?NAME required}; [[ -f $d/display ]] || die "no running sandbox '$1' (scripts/ui-sandbox.sh start $1)"; echo "$d"; }
xdo() { local d; d=$(dir_of "$1"); shift; DISPLAY=$(<"$d/display") xdotool "$@"; }

start() {
    local name=${1:-default} geom=${2:-1600x1000}
    [[ $name =~ ^[A-Za-z0-9_-]+$ ]] || die "NAME may hold only letters, digits, - and _"
    [[ $geom =~ ^([0-9]+)x([0-9]+)$ ]] || die "size must look like 1600x1000"
    local w=${BASH_REMATCH[1]} h=${BASH_REMATCH[2]}
    for t in kwin_wayland dbus-run-session xdotool spectacle setsid systemd-run systemctl choom; do need "$t"; done
    local bin=${VE_BIN:-}
    if [[ -z $bin ]]; then
        bin=$ROOT/target/release/view-everything
        [[ -x $bin ]] || bin=$ROOT/target/debug/view-everything
    fi
    [[ -x $bin ]] || die "no binary at $bin; run cargo build -p ve-app or set VE_BIN"

    local d=$BASE/$name
    if [[ -f $d/pid ]] && kill -0 "$(<"$d/pid")" 2>/dev/null; then die "sandbox '$name' is already running"; fi
    rm -rf "$d"
    mkdir -p "$d"/xdg/{config,data,cache,state}
    chmod 700 "$BASE" "$d"
    # Without this, XTEST input from xdotool waits on a permission dialog inside the sandbox.
    printf '[Xwayland]\nXwaylandEisNoPromptApps=xdotool\n' >"$d/xdg/config/kwinrc"
    cat >"$d/app.sh" <<EOF
#!/bin/sh
echo "\$DBUS_SESSION_BUS_ADDRESS" > '$d/bus'
echo "\$DISPLAY" > '$d/display.tmp' && mv '$d/display.tmp' '$d/display'
unset WAYLAND_DISPLAY
exec '$bin' > '$d/app.log' 2>&1
EOF
    chmod +x "$d/app.sh"
    echo "wayland-ve-$name" >"$d/socket"

    unset DISPLAY WAYLAND_DISPLAY
    XDG_CONFIG_HOME=$d/xdg/config XDG_DATA_HOME=$d/xdg/data XDG_CACHE_HOME=$d/xdg/cache XDG_STATE_HOME=$d/xdg/state \
    KWIN_SCREENSHOT_NO_PERMISSION_CHECKS=1 \
        systemd-run --user --scope --quiet --collect --unit="ve-ui-sandbox-$name" \
            -p MemoryHigh="${VE_SANDBOX_MEMORY_HIGH:-1536M}" -p MemoryMax="${VE_SANDBOX_MEMORY_MAX:-2G}" \
            -p MemorySwapMax="${VE_SANDBOX_SWAP_MAX:-1G}" -p OOMPolicy=kill -p TimeoutStopSec=5s -- \
        choom -n 1000 -- \
        setsid dbus-run-session -- kwin_wayland --virtual --xwayland --no-lockscreen \
            --width "$w" --height "$h" --socket "wayland-ve-$name" --exit-with-session "$d/app.sh" \
            >"$d/kwin.log" 2>&1 </dev/null &
    echo $! >"$d/pid"

    local i
    for i in $(seq 150); do
        if [[ -f $d/display ]] && DISPLAY=$(<"$d/display") xdotool search --name 'View Everything' >/dev/null 2>&1; then
            sleep 1 # let the first frame land
            echo "sandbox '$name' ready: display $(<"$d/display"), ${w}x${h}, logs in $d"
            return
        fi
        kill -0 "$(<"$d/pid")" 2>/dev/null || { tail -20 "$d/kwin.log" >&2; die "sandbox '$name' exited during start"; }
        sleep 0.2
    done
    stop "$name"
    die "the app window did not appear within 30 s"
}

stop() {
    local d=$BASE/${1:?NAME required}
    [[ -d $d ]] || die "no sandbox '$1'"
    # The scope holds everything the sandbox started: KWin, Xwayland, the app, the bus and any
    # services the bus activated. Stopping it sends SIGTERM, then SIGKILL after 5 s.
    systemctl --user stop "ve-ui-sandbox-$1.scope" 2>/dev/null || true
    rm -rf "$d"
    echo "sandbox '$1' stopped"
}

shot() {
    local d; d=$(dir_of "$1"); local out=${2:?FILE.png required}
    mkdir -p "$(dirname "$out")"
    rm -f "$out"
    DBUS_SESSION_BUS_ADDRESS=$(<"$d/bus") WAYLAND_DISPLAY=$(<"$d/socket") QT_QPA_PLATFORM=wayland \
        timeout 30 spectacle --background --nonotify --fullscreen --output "$out" >/dev/null 2>&1 || true
    [[ -s $out ]] || die "screenshot failed"
    echo "$out"
}

cmd=${1:-}; shift || true
case $cmd in
    start) start "$@" ;;
    stop) stop "$@" ;;
    shot) shot "$@" ;;
    click)
        (($# >= 3)) || die "usage: click NAME X Y [BUTTON]"
        # An immediate move+click can be swallowed; settle on the target first.
        xdo "$1" mousemove "$2" "$3"; sleep 0.3; xdo "$1" click "${4:-1}"; sleep 0.3 ;;
    key) (($# >= 2)) || die "usage: key NAME KEYS..."; n=$1; shift; xdo "$n" key "$@" ;;
    type) (($# == 2)) || die "usage: type NAME TEXT"; xdo "$1" type --delay 20 -- "$2" ;;
    xdo) (($# >= 2)) || die "usage: xdo NAME ARGS..."; xdo "$@" ;;
    list) for d in "$BASE"/*/; do [[ -f $d/pid ]] && kill -0 "$(<"$d/pid")" 2>/dev/null && basename "$d"; done 2>/dev/null || true ;;
    *) sed -n '2,20p' "$0"; exit 2 ;;
esac
