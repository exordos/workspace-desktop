#!/usr/bin/env bash
# Проверяет историю и байты вложения после перезапуска из другого worktree.
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
other=$(cd -- "${1:?Передай путь ко второму worktree с командой cargo stand}" && pwd -P)
[[ "$root" != "$other" ]] || { printf '%s\n' "Нужны два разных worktree." >&2; exit 1; }
state="${XDG_DATA_HOME:-$HOME/.local/share}/workspace-desktop/stand"

attachment() {
    docker compose --env-file "$state/runtime.env" -f "$state/compose.yaml" \
        exec -T iam python - "$@" < "$root/tests/workspace-stand-attachment.py"
}

cd "$root"
stand=$(cargo stand)
message=$(attachment send)
cargo stand restart

cd "$other"
reused=$(cargo stand)
[[ "$stand" == "$reused" ]] || { printf '%s\n' "Второй worktree получил другой стенд." >&2; exit 1; }
attachment open "$message"
