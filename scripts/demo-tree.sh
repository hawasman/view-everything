#!/usr/bin/env bash
# Builds a fake but realistic folder tree under DIR for screenshots and demos: no personal data,
# about 450 MB of real (incompressible) bytes, varied modification times, and a few Duplicate
# groups. Safe to re-run: the tree is rebuilt from scratch. Prints DIR.
#
#   scripts/demo-tree.sh DIR
#   VE_SANDBOX_VOLUME=DIR scripts/ui-sandbox.sh start NAME   # Index only this tree
set -euo pipefail

dir=${1:?usage: demo-tree.sh DIR}
[[ $dir == / || $dir == "$HOME" ]] && { echo "demo-tree: refusing to use $dir" >&2; exit 1; }
rm -rf "${dir:?}"
mkdir -p "$dir"
cd "$dir"

# blob FILE MB [DAYS_OLD]: MB of random bytes, last modified DAYS_OLD days ago.
blob() {
    mkdir -p "$(dirname "$1")"
    head -c "$(($2 * 1024 * 1024))" /dev/urandom >"$1"
    touch -d "${3:-3} days ago" "$1"
}
# small FILE KB [DAYS_OLD]
small() {
    mkdir -p "$(dirname "$1")"
    head -c "$(($2 * 1024))" /dev/urandom >"$1"
    touch -d "${3:-3} days ago" "$1"
}
text() { mkdir -p "$(dirname "$1")"; printf '%s\n' "$2" >"$1"; touch -d "${3:-3} days ago" "$1"; }

# Rust project, build output untouched for months (Cleaner pre-ticks it).
text Projects/ledger-rs/Cargo.toml $'[package]\nname = "ledger-rs"\nversion = "0.4.2"\nedition = "2021"\n\n[dependencies]\nserde = "1"\ntokio = { version = "1", features = ["full"] }' 40
text Projects/ledger-rs/Cargo.lock "# generated" 40
for f in main lib parser report; do text "Projects/ledger-rs/src/$f.rs" "// $f" 35; done
blob Projects/ledger-rs/target/debug/deps/libtokio-3f9a1c.rlib 24 120
blob Projects/ledger-rs/target/debug/deps/libserde-9b27e4.rlib 14 120
blob Projects/ledger-rs/target/debug/incremental/ledger_rs-1q2w3e/s-gh12-abc.bin 22 120
blob Projects/ledger-rs/target/debug/ledger-rs 20 120
blob Projects/ledger-rs/target/release/deps/libtokio-71c0de.rlib 26 95
blob Projects/ledger-rs/target/release/ledger-rs 9 95

# Node project with dependencies and a Next.js build.
text Projects/storefront/package.json $'{\n  "name": "storefront",\n  "version": "1.2.0",\n  "scripts": { "dev": "next dev", "build": "next build" }\n}' 30
text Projects/storefront/package-lock.json "{}" 30
text Projects/storefront/src/app/page.tsx "export default function Page() {}" 30
blob Projects/storefront/node_modules/next/dist/compiled/next-server.js 18 110
blob Projects/storefront/node_modules/react-dom/cjs/react-dom.development.js 9 110
blob Projects/storefront/node_modules/typescript/lib/typescript.js 24 110
blob Projects/storefront/node_modules/@swc/core-linux-x64-gnu/swc.node 28 110
for i in $(seq 1 30); do small "Projects/storefront/node_modules/lodash/fp/f$i.js" 64 110; done
blob Projects/storefront/.next/cache/webpack/client-production/0.pack 30 100
blob Projects/storefront/.next/server/chunks/vendor.js 12 100

# Games: a few big files.
blob Games/Starfall/Starfall.pak 44 20
blob Games/Starfall/Content/music.bank 24 20
blob Games/Starfall/Content/textures.bin 22 20
blob Games/Hollow-Cove/data.dat 30 60
blob Games/Hollow-Cove/save/slot1.sav 2 2

# Videos
blob Videos/Holiday-2024.mp4 28 200
blob Videos/Conference-talk.mkv 20 150
blob Videos/Birthday.mov 18 90
blob Videos/Clips/clip-001.mp4 7 8
blob Videos/Clips/clip-002.mp4 6 8

# Photos: many small files across a few trips.
for trip in Lisbon-2024 Alps-2023 Home; do
    for i in $(seq 1 60); do small "Photos/$trip/IMG_$(printf %04d "$i").jpg" $((120 + RANDOM % 380)) $((RANDOM % 700 + 5)); done
done

# Documents
text Documents/Taxes/2024-summary.txt "Totals" 250
blob Documents/Reports/annual-report.pdf 6 160
blob Documents/Reports/slides.pptx 9 60
blob Documents/Books/rust-in-practice.epub 3 300
for i in 1 2 3 4 5 6 7 8; do small "Documents/Notes/note-$i.md" 8 $((i * 20)); done

# Cache and temp folders.
blob .cache/thumbnails/large/cache.db 22 45
blob .cache/pip/wheels/numpy-1.26-cp312.whl 14 70
blob .cache/mozilla/firefox/cache2/entries/a1b2c3 11 12
blob .cache/yarn/v6/npm-react-18.tgz 6 80
for i in $(seq 1 25); do small ".cache/thumbnails/normal/$i.png" 40 $((i * 3)); done
blob Downloads/installer-setup.iso 12 140

# Duplicate groups: identical content, at least 1 MB each.
blob .dup-src/backup-key.bin 12 70 && mkdir -p Documents/Backups && cp -p .dup-src/backup-key.bin Documents/Backups/photos-2023.zip
cp -p .dup-src/backup-key.bin "Downloads/photos-2023 (1).zip"
blob .dup-src/talk.bin 16 130 && cp -p .dup-src/talk.bin Videos/Conference-talk-copy.mkv && cp -p .dup-src/talk.bin Downloads/talk-final.mkv
blob .dup-src/album.bin 8 50 && cp -p .dup-src/album.bin Documents/Books/album.flac && cp -p .dup-src/album.bin Games/Starfall/Content/album.flac
rm -rf .dup-src

# An empty Trash on this folder's Volume (uid 0 is what ui-sandbox.sh's VE_SANDBOX_VOLUME runs as),
# so the Dashboard shows 0 B instead of waiting for a Trash size.
for uid in 0 "$(id -u)"; do mkdir -p ".Trash-$uid/files" ".Trash-$uid/info"; done

echo "$dir"
