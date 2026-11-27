#!/usr/bin/env bash
set -euo pipefail

if [[ $# != 2 || $(uname -s) != Linux || $(uname -m) != x86_64 ]]; then
    echo 'Usage on Linux x86_64: bash scripts/bundle-workspace-desktop-linux.sh EXECUTABLE OUTPUT.tar.gz' >&2
    exit 1
fi

executable=$1
output=$2
if [[ ! -x "$executable" || "$output" != *.tar.gz ]]; then
    echo 'Expected a built executable and a .tar.gz output path.' >&2
    exit 1
fi

staging=$(mktemp -d)
trap 'rm -rf -- "$staging"' EXIT
package=workspace-desktop-linux-x64
mkdir "$staging/$package"
install -m 755 "$executable" "$staging/$package/workspace-desktop"
cat > "$staging/$package/README.txt" <<'EOF'
Workspace Desktop for Linux x86_64

Extract the archive and run ./workspace-desktop from a Wayland or X11 session.
The build targets Ubuntu 24.04 and uses Vulkan and Secret Service. For a minimal
Ubuntu desktop, install the runtime dependencies if they are missing:

  sudo apt install libdbus-1-3 libfontconfig1 libfreetype6 libvulkan1 \
    mesa-vulkan-drivers libwayland-client0 libwayland-cursor0 libx11-6 \
    libx11-xcb1 libxcb1 libxkbcommon0 libxkbcommon-x11-0 gnome-keyring

Signing in and restoring accounts requires a running, unlocked Secret Service
in the desktop session, such as GNOME Keyring or KDE Wallet.

This is a preview build. Native notifications, tray, HEIF preview and video
playback are still pending on Linux.
EOF

tar -czf "$output" -C "$staging" "$package"
mkdir "$staging/verify"
tar -xzf "$output" -C "$staging/verify"
binary="$staging/verify/$package/workspace-desktop"
header=$(LC_ALL=C readelf -h "$binary")
if [[ "$header" != *'Advanced Micro Devices X86-64'* ]]; then
    echo 'Expected an x86-64 ELF binary.' >&2
    exit 1
fi
help=$("$binary" --help)
[[ "$help" == 'Usage: workspace-desktop '* ]]
