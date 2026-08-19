#!/bin/bash

# Rebuild a Tauri-generated Carrel DMG with the first-run read-me at its top
# level. (An executable "Open Carrel.command" helper shipped in 3.0.6 and was
# removed again: files inside a downloaded DMG inherit its quarantine flag, and
# macOS 15+ offers no right-click -> Open override for unsigned items, so
# Gatekeeper blocked the helper with only "Move to Trash" -- the exact dialog
# it existed to avoid. Without notarization, no executable we ship can run.)
# This runs only on the macOS release jobs, after Tauri has created its normal
# DMG and before the enhanced copy replaces the release asset on GitHub.

set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "Usage: $0 path/to/Carrel.dmg" >&2
  exit 2
fi

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This script requires macOS (hdiutil and ditto)." >&2
  exit 1
fi

for command in hdiutil ditto; do
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "Required command not found: $command" >&2
    exit 1
  fi
done

input_dmg="$1"
if [[ ! -f "$input_dmg" ]]; then
  echo "DMG not found: $input_dmg" >&2
  exit 1
fi

dmg_dir="$(cd "$(dirname "$input_dmg")" && pwd)"
dmg_path="$dmg_dir/$(basename "$input_dmg")"
repo_root="$(cd "$(dirname "$0")/.." && pwd)"
assets_dir="$repo_root/packaging/macos"
readme="$assets_dir/Read Me First.txt"

if [[ ! -f "$readme" ]]; then
  echo "Required DMG asset not found: $readme" >&2
  exit 1
fi

work_dir="$(mktemp -d "${TMPDIR:-/tmp}/carrel-dmg.XXXXXX")"
source_mount="$work_dir/source-volume"
staging="$work_dir/staging"
compressed_dmg="$work_dir/Carrel-enhanced.dmg"
source_attached=false

cleanup() {
  if [[ "$source_attached" == true ]]; then
    hdiutil detach "$source_mount" -quiet >/dev/null 2>&1 || true
  fi
  rm -rf "$work_dir"
}
trap cleanup EXIT

mkdir -p "$source_mount" "$staging"

# Copy Tauri's complete volume, including its Applications symlink,
# background image, icons, and Finder metadata.
echo "Copying Tauri DMG contents..."
hdiutil attach "$dmg_path" -readonly -nobrowse -noverify \
  -mountpoint "$source_mount" -quiet
source_attached=true
ditto "$source_mount" "$staging"
hdiutil detach "$source_mount" -quiet
source_attached=false

# Drop the Finder aesthetics rather than trying to extend them to the read-me.
# Tauri bundles create-dmg's bundle_dmg.sh, where icon positions and the
# window background are applied *only* by an `osascript` step — and Tauri
# passes create-dmg's `--skip-jenkins` whenever it detects CI (unless
# TAURI_BUNDLER_DMG_IGNORE_CI is set), which skips that step. So a release
# DMG arrives with an unarranged window and a .background image that was
# never wired up; there is no designed layout here to preserve. A local
# `tauri build` on a GUI session *does* arrange it, and this discards that —
# deliberately, since positioning the extra item would mean either driving
# Finder ourselves (the flakiness upstream added the flag for) or writing
# .DS_Store by hand. Let Finder auto-arrange the top-level items instead.
rm -f "$staging/.DS_Store"
rm -rf "$staging/.background"

ditto "$readme" "$staging/Read Me First.txt"

echo "Creating enhanced DMG..."
hdiutil create -srcfolder "$staging" -volname "Carrel" -fs HFS+ \
  -format UDZO -imagekey zlib-level=9 -ov "$compressed_dmg" -quiet

# Keep the original filename so tauri-action's uploaded asset can be replaced
# with `gh release upload --clobber` without changing download links.
mv "$compressed_dmg" "$dmg_path"
echo "Added macOS first-run help to $dmg_path"
