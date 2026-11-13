#!/bin/sh
set -eu

project_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
executable=${1:-"$project_dir/target/debug/workspace-desktop"}

if [ "$(uname -s)" != Darwin ] || [ ! -x "$executable" ]; then
    printf '%s\n' 'Соберите приложение на macOS командой cargo build -p workspace-desktop --bin workspace-desktop.' >&2
    exit 1
fi

app=$(dirname -- "$executable")'/Workspace Desktop.app'
mkdir -p "$app/Contents/MacOS"
cp "$executable" "$app/Contents/MacOS/workspace-desktop"
cp "$project_dir/crates/workspace-desktop/macos/Info.plist" "$app/Contents/Info.plist"
codesign --force --sign - "$app" >&2
printf '%s\n' "$app"
