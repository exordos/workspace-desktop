#!/bin/sh
set -eu

project_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$project_dir"

version=2.8.0
case "$(uname -s)-$(uname -m)" in
    Darwin-arm64)
        platform=aarch64-apple-darwin
        sha256=8da8bdbfb8568138616a082c05a87224e08d5cb2994feb956426291cf0c6fe10
        ;;
    Darwin-x86_64)
        platform=x86_64-apple-darwin
        sha256=b7f694ad19c1cf168388f3295a5e825425f7dac5cd27cffd2d89a51ed4ab0f22
        ;;
    *)
        printf '%s\n' 'Dev-режим Workspace Desktop доступен на macOS.' >&2
        exit 1
        ;;
esac

release="watchexec-$version-$platform"
tool_dir="$project_dir/target/dev-tools/$release"
watcher="$tool_dir/watchexec"

if [ ! -x "$watcher" ]; then
    # Публикуем готовый бинарник одним перемещением после проверки архива.
    mkdir -p "$tool_dir"
    (
        download_dir=$(mktemp -d "$tool_dir/.download.XXXXXX")
        archive="$release.tar.xz"
        trap 'rm -f "$download_dir/$archive" "$download_dir/watchexec"; rmdir "$download_dir"' EXIT
        trap 'exit 1' HUP INT TERM
        cd "$download_dir"

        printf 'Скачиваю watchexec %s для %s.\n' "$version" "$platform" >&2
        curl --fail --silent --show-error --location --connect-timeout 10 --max-time 60 \
            --output "$archive" "https://github.com/watchexec/watchexec/releases/download/v$version/$archive"
        if ! printf '%s  %s\n' "$sha256" "$archive" | shasum -a 256 --check --status; then
            printf '%s\n' 'Контрольная сумма архива watchexec не совпала.' >&2
            exit 1
        fi

        tar -xOf "$archive" "$release/watchexec" > watchexec
        chmod +x watchexec
        mv -f watchexec "$watcher"
    )
fi

exec "$watcher" --restart --shell=none --project-origin "$project_dir" \
    --watch crates --watch-non-recursive . \
    --filter 'crates/**' --filter '/Cargo.toml' --filter '/Cargo.lock' \
    --filter '/rust-toolchain.toml' \
    -- cargo run -p workspace-desktop --bin workspace-desktop
