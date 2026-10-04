#!/bin/sh
# Packs dist/shadowpty.mcpb from the release binaries.
#
#   scripts/pack-mcpb.sh <binaries-dir> <version>
#
# <binaries-dir> holds shadowpty-<target> files, as the release build names them. Targets
# missing from it are left out of the bundle (handy for a local build of one platform).
set -eu

bins=${1:?usage: scripts/pack-mcpb.sh <binaries-dir> <version>}
version=${2:?usage: scripts/pack-mcpb.sh <binaries-dir> <version>}
root=$(cd "$(dirname "$0")/.." && pwd)
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

mkdir -p "$stage/server/bin" "$root/dist"
cp "$root/mcpb/server/shadowpty" "$stage/server/shadowpty"
cp "$root/assets/logo.png" "$stage/icon.png"
cp "$root/LICENSE-MIT" "$root/LICENSE-APACHE" "$stage/"

found=0
for target in aarch64-apple-darwin x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do
  if [ -f "$bins/shadowpty-$target" ]; then
    cp "$bins/shadowpty-$target" "$stage/server/bin/"
    found=$((found + 1))
  else
    echo "pack-mcpb: no binary for $target, leaving it out" >&2
  fi
done
[ "$found" -gt 0 ] || { echo "pack-mcpb: no binaries in $bins" >&2; exit 1; }
chmod 755 "$stage/server/shadowpty" "$stage"/server/bin/*

sed "s/\"version\": \"[^\"]*\"/\"version\": \"$version\"/" "$root/mcpb/manifest.json" \
  >"$stage/manifest.json"

npx -y @anthropic-ai/mcpb@2.1.2 validate "$stage/manifest.json"
npx -y @anthropic-ai/mcpb@2.1.2 pack "$stage" "$root/dist/shadowpty.mcpb"
