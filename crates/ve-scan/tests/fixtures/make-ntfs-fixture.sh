#!/bin/sh
# Regenerates small.ntfs.gz: an 8 MiB NTFS image holding
#   /tiny.txt   "hello ntfs\n"          (11 bytes, resident data)
#   /big.bin    300000 bytes            (non-resident data, byte i = i % 251)
#   /empty.dat  0 bytes
# Needs mkntfs and ntfscp (ntfs-3g); no root required.
set -eu
cd "$(dirname "$0")"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
printf 'hello ntfs\n' > "$tmp/tiny.txt"
LC_ALL=C awk 'BEGIN { for (i = 0; i < 300000; i++) printf "%c", i % 251 }' > "$tmp/big.bin"
: > "$tmp/empty.dat"
truncate -s 8M "$tmp/img"
mkntfs -F -q -Q "$tmp/img" >/dev/null
for f in tiny.txt big.bin empty.dat; do ntfscp "$tmp/img" "$tmp/$f" "/$f" >/dev/null; done
gzip -9 -n -c "$tmp/img" > small.ntfs.gz
