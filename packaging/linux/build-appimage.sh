#!/usr/bin/env bash
# Builds view-everything-x86_64.AppImage into dist/ (repo root).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cache="${XDG_CACHE_HOME:-$HOME/.cache}/view-everything"
appdir="$root/target/appimage/AppDir"
out="$root/dist"

cd "$root"
cargo build --release -p ve-app --locked

rm -rf "$appdir"
mkdir -p "$appdir/usr/bin" "$out"
install -m755 target/release/view-everything "$appdir/usr/bin/view-everything"
install -m644 packaging/linux/view-everything.desktop "$appdir/view-everything.desktop"
install -m644 packaging/icons/view-everything.svg "$appdir/view-everything.svg"
ln -s view-everything.svg "$appdir/.DirIcon"
cat > "$appdir/AppRun" <<'EOF'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/view-everything" "$@"
EOF
chmod +x "$appdir/AppRun"

tool="$(command -v appimagetool || true)"
if [ -z "$tool" ]; then
    mkdir -p "$cache"
    tool="$cache/appimagetool-x86_64.AppImage"
    if [ ! -x "$tool" ]; then
        curl -fL -o "$tool.part" \
            https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
        chmod +x "$tool.part"
        mv "$tool.part" "$tool"
    fi
fi

# --appimage-extract-and-run: works where FUSE is unavailable (CI, containers).
ARCH=x86_64 "$tool" --appimage-extract-and-run "$appdir" "$out/view-everything-x86_64.AppImage"
echo "Wrote $out/view-everything-x86_64.AppImage"
