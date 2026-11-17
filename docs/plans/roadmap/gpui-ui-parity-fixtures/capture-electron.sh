#!/bin/sh
# Снимает фиксированные состояния на отдельном дереве исходников оригинала.
set -eu

if [ "$#" -ne 2 ]; then
  echo "Использование: $0 ЧИСТЫЙ_WORKTREE_WORKSPACE_UI НОВЫЙ_КАТАЛОГ_КАДРОВ" >&2
  exit 2
fi

fixture_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd -P)
original=$(CDPATH='' cd -- "$1" && pwd -P)
mkdir -- "$2"
output=$(CDPATH='' cd -- "$2" && pwd -P)
commit=af0999886daaffefc2a7be33f8dcf3f484a0e9c1
if [ "$(git -C "$original" rev-parse HEAD)" != "$commit" ] ||
   [ -n "$(git -C "$original" status --porcelain)" ]; then
  echo "Для съёмки нужен чистый worktree workspace_ui на $commit" >&2
  exit 1
fi

# Файлы находятся в отдельном каталоге, чтобы найти исходные helpers и зависимости npm.
staging=$(mktemp -d "$original/.ui-parity.XXXXXX")
trap 'rm -rf -- "$staging"' EXIT
cp "$fixture_dir/scenes.json" "$fixture_dir/electron-capture.mjs" "$fixture_dir/electron-scenario.ts" "$staging/"
cd "$original"
node "$staging/electron-capture.mjs" "$output"
