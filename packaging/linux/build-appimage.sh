#!/usr/bin/env bash
# Builds view-everything-x86_64.AppImage (and its .zsync) into dist/ (repo root). Needs rsvg-convert (librsvg)
# for the PNG icons, and zsyncmake (zsync) for the .zsync file.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cache="${XDG_CACHE_HOME:-$HOME/.cache}/view-everything"
appdir="$root/target/appimage/AppDir"
out="$root/dist"

cd "$root"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
cargo build --release -p ve-app --locked

rm -rf "$appdir"
icons="$appdir/usr/share/icons/hicolor"
mkdir -p "$appdir/usr/bin" "$icons/scalable/apps" "$icons/256x256/apps" "$out"
install -m755 target/release/view-everything "$appdir/usr/bin/view-everything"
# App stores and launchers read the version from the desktop entry (Shelly shows "Unknown" without it).
{ cat packaging/linux/view-everything.desktop; echo "X-AppImage-Version=$version"; } > "$appdir/view-everything.desktop"
install -m644 packaging/icons/view-everything.svg "$icons/scalable/apps/view-everything.svg"
rsvg-convert -w 256 -h 256 packaging/icons/view-everything.svg -o "$icons/256x256/apps/view-everything.png"
ln -s usr/share/icons/hicolor/scalable/apps/view-everything.svg "$appdir/view-everything.svg"
# The AppDir spec has .DirIcon as a PNG, for thumbnailers and launchers that read no SVG.
ln -s usr/share/icons/hicolor/256x256/apps/view-everything.png "$appdir/.DirIcon"
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

# Update information (ADR-0007): Shelly, AppImageUpdate and Gear Lever read it from the AppImage and fetch the
# .zsync published with each release. appimagetool writes that file next to the AppImage when zsyncmake exists.
update="gh-releases-zsync|hawasman|view-everything|latest|view-everything-x86_64.AppImage.zsync"
command -v zsyncmake >/dev/null || echo "warning: zsyncmake not found, no .zsync file is written (install zsync)" >&2
# --appimage-extract-and-run: works where FUSE is unavailable (CI, containers).
(cd "$out" && ARCH=x86_64 "$tool" --appimage-extract-and-run --updateinformation "$update" "$appdir" view-everything-x86_64.AppImage)
echo "Wrote $out/view-everything-x86_64.AppImage"
