#!/bin/sh
set -eu

# Проверяем путь от cargo dev до перезапуска на минимальном тестовом приложении.
project_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cargo_bin=$(command -v cargo)
fixture=$(mktemp -d)
watcher_pid=
cleanup() {
    if [ -n "$watcher_pid" ]; then
        kill -TERM "$watcher_pid" 2>/dev/null || true
        wait "$watcher_pid" || true
    fi
    rm -rf -- "$fixture"
}
trap cleanup EXIT

mkdir -p "$fixture/.cargo" "$fixture/tools/workspace-dev/src" "$fixture/scripts" "$fixture/bin" \
    "$fixture/crates/workspace-desktop/src" "$fixture/target"
cp "$project_dir/.cargo/config.toml" "$fixture/.cargo/"
cp "$project_dir/tools/workspace-dev/Cargo.toml" "$project_dir/tools/workspace-dev/Cargo.lock" \
    "$fixture/tools/workspace-dev/"
cp "$project_dir/tools/workspace-dev/src/main.rs" "$fixture/tools/workspace-dev/src/"
cp "$project_dir/scripts/dev-workspace-desktop.sh" "$fixture/scripts/"
cp "$project_dir/rust-toolchain.toml" "$fixture/"
source_file="$fixture/crates/workspace-desktop/src/lib.rs"
printf '// Исходная версия.\n' > "$source_file"
printf '[workspace]\nmembers = ["crates/workspace-desktop"]\nresolver = "3"\n' > "$fixture/Cargo.toml"
cat > "$fixture/crates/workspace-desktop/Cargo.toml" <<'EOF'
[package]
name = "workspace-desktop"
version = "0.1.0"
edition = "2024"
EOF
cat > "$fixture/crates/workspace-desktop/src/main.rs" <<'EOF'
//! Сохраняет PID тестового приложения, чтобы проверить его перезапуск.

use std::fs::OpenOptions;
use std::io::Write;

/// Записывает PID и ждёт, пока наблюдатель завершит процесс.
fn main() -> std::io::Result<()> {
    let mut launches = OpenOptions::new().create(true).append(true).open("launches")?;
    writeln!(launches, "{}", std::process::id())?;
    std::thread::sleep(std::time::Duration::from_secs(60));
    Ok(())
}
EOF
"$cargo_bin" generate-lockfile --offline --manifest-path "$fixture/Cargo.toml"

start_watcher() {
    (
        cd "$fixture"
        export PATH="$fixture/bin:$(dirname -- "$cargo_bin"):/usr/bin:/bin:/usr/sbin:/sbin"
        export CARGO_TARGET_DIR="$fixture/target"
        exec "$cargo_bin" dev
    ) > "$fixture/watch.log" 2>&1 &
    watcher_pid=$!
}

wait_for_launch() {
    for attempt in $(seq 1 "${2:-100}"); do
        launched_pid=$(sed -n "${1}p" "$fixture/launches" 2>/dev/null || true)
        [ -z "$launched_pid" ] || return 0
        kill -0 "$watcher_pid" 2>/dev/null || break
        sleep 0.1
    done
    cat "$fixture/watch.log" >&2
    printf 'Не дождались запуска %s.\n' "$1" >&2
    exit 1
}

start_watcher
wait_for_launch 1 700
first_pid=$launched_pid
printf 'build output\n' > "$fixture/target/output"
sleep 1
[ "$(wc -l < "$fixture/launches")" -eq 1 ]

# Сохранение через замену файла, как в редакторе.
printf '// Изменённая версия.\n' > "$source_file.tmp"
mv "$source_file.tmp" "$source_file"
wait_for_launch 2
if kill -0 "$first_pid" 2>/dev/null; then
    printf '%s\n' 'Первый процесс остался запущен.' >&2
    exit 1
fi

# Изменение манифеста workspace тоже запускает сборку.
printf '\n# updated\n' >> "$fixture/Cargo.toml"
wait_for_launch 3

# При остановке наблюдения дочерний процесс тоже должен завершиться.
kill -TERM "$watcher_pid"
wait "$watcher_pid"
watcher_pid=
if kill -0 "$launched_pid" 2>/dev/null; then
    printf '%s\n' 'Последний процесс остался запущен.' >&2
    exit 1
fi

# Повторный запуск должен использовать локальный бинарник без скачивания.
printf '#!/bin/sh\nexit 1\n' > "$fixture/bin/curl"
chmod +x "$fixture/bin/curl"
start_watcher
wait_for_launch 4
