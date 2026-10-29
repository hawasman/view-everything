#!/usr/bin/env bash
# Builds a universal (Apple Silicon + Intel) "View Everything.app" into
# dist/view-everything-macos-universal.dmg (repo root). Needs Xcode command line tools and
# rsvg-convert (`brew install librsvg`) for the icon. The app is ad-hoc signed, not notarized:
# on first launch, right-click it and choose Open.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
work="$root/target/macos"
app="$work/View Everything.app"
out="$root/dist"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"

cd "$root"
targets=(aarch64-apple-darwin x86_64-apple-darwin)
for target in "${targets[@]}"; do
    rustup target add "$target" >/dev/null
    cargo build --release --locked -p ve-app --target "$target"
done

rm -rf "$work"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$out"
lipo -create -output "$app/Contents/MacOS/view-everything" \
    target/aarch64-apple-darwin/release/view-everything target/x86_64-apple-darwin/release/view-everything

iconset="$work/view-everything.iconset"
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
    rsvg-convert -w "$size" -h "$size" packaging/icons/view-everything.svg -o "$iconset/icon_${size}x${size}.png"
    rsvg-convert -w $((size * 2)) -h $((size * 2)) packaging/icons/view-everything.svg -o "$iconset/icon_${size}x${size}@2x.png"
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/view-everything.icns"

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>View Everything</string>
    <key>CFBundleDisplayName</key><string>View Everything</string>
    <key>CFBundleIdentifier</key><string>org.view-everything.app</string>
    <key>CFBundleExecutable</key><string>view-everything</string>
    <key>CFBundleIconFile</key><string>view-everything</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleVersion</key><string>${version}</string>
    <key>CFBundleShortVersionString</key><string>${version}</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF

# Apple Silicon refuses to run unsigned code; an ad-hoc signature is enough to launch.
codesign --force --deep --sign - "$app"

staging="$work/dmg"
mkdir -p "$staging"
cp -R "$app" "$staging/"
ln -s /Applications "$staging/Applications"
hdiutil create -volname "View Everything" -srcfolder "$staging" -ov -format UDZO \
    "$out/view-everything-macos-universal.dmg"
echo "Wrote $out/view-everything-macos-universal.dmg"
