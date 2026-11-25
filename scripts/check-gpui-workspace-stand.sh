#!/usr/bin/env bash
# Запускает сквозной GPUI-сценарий на локальном Workspace с двумя аккаунтами.
set +x
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

# Только один такой прогон может менять общий стендовый чат A/B за раз.
if [[ "${1-}" != "--locked" ]]; then
    exec python3 - "$0" <<'PY'
import fcntl
import os
import subprocess
import sys

lock_path = f"/tmp/workspace-desktop-gpui-stand-{os.getuid()}.lock"
fd = os.open(lock_path, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
try:
    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
except BlockingIOError:
    sys.exit("GPUI-проверка общего стенда уже выполняется")
sys.exit(subprocess.call(["bash", sys.argv[1], "--locked"]))
PY
fi

# Стенд общий для worktree; команда переиспользует работающий экземпляр.
cargo stand

# Подготавливаем непрочитанное B только в личном чате стендовых A/B.
stand_state="${XDG_DATA_HOME:-$HOME/.local/share}/workspace-desktop/stand"
docker compose --env-file "$stand_state/runtime.env" -f "$stand_state/compose.yaml" \
    exec -T iam python - < "$root/tests/workspace-stand-prepare-read.py"

# cargo stand env печатает пароль. Подставляем его в окружение, не в журнал.
stand_a=$(cargo stand env A)
eval "$stand_a"
stand_b=$(cargo stand env B)
stand_b_exports=$(
    eval "$stand_b"
    printf 'export WORKSPACE_TEST_LOGIN_B=%q\n' "$WORKSPACE_TEST_LOGIN_A"
    printf 'export WORKSPACE_TEST_PASSWORD_B=%q\n' "$WORKSPACE_TEST_PASSWORD_A"
)
eval "$stand_b_exports"
unset stand_a stand_b stand_b_exports

test_name=shared_messenger_real_workspace
test_list=$(cargo test --locked -p workspace-desktop --features gpui-test --lib -- --list)
matches=$(printf '%s\n' "$test_list" | grep -Ec "(^|::)${test_name}: test$" || true)
if [[ "$matches" != 1 ]]; then
    printf 'Ожидался один тест %s, найдено: %s\n' "$test_name" "$matches" >&2
    exit 1
fi

test_log=$(mktemp "${TMPDIR:-/tmp}/workspace-stand-test.XXXXXX")
trap 'rm -f "$test_log"' EXIT
cargo test --locked -p workspace-desktop --features gpui-test --lib \
    "$test_name" -- --ignored --nocapture | tee "$test_log"
if ! grep -Eq "^test .*::${test_name} \.\.\. ok$" "$test_log" || \
   ! grep -Eq '^test result: ok\. 1 passed;' "$test_log"; then
    printf 'Тест %s не был успешно выполнен ровно один раз.\n' "$test_name" >&2
    exit 1
fi
