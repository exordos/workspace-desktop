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
    "$fixture/crates/workspace-desktop/src" "$fixture/crates/workspace-desktop/examples" "$fixture/target"
cp "$project_dir/.cargo/config.toml" "$fixture/.cargo/"
cp "$project_dir/tools/workspace-dev/Cargo.toml" "$project_dir/tools/workspace-dev/Cargo.lock" \
    "$fixture/tools/workspace-dev/"
cp "$project_dir/tools/workspace-dev/src/main.rs" "$fixture/tools/workspace-dev/src/"
cp "$project_dir/scripts/dev-workspace-desktop.sh" "$project_dir/scripts/run-workspace-desktop-dev.sh" \
    "$fixture/scripts/"
cp "$project_dir/rust-toolchain.toml" "$fixture/"
# Берём уже загруженный проверенный наблюдатель, чтобы проверка не зависела от сети.
if [ -d "$project_dir/target/dev-tools" ]; then
    cp -R "$project_dir/target/dev-tools" "$fixture/target/"
fi
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
//! Сохраняет PID и режим запуска, чтобы проверить перезапуск и изоляцию профиля.

use std::fs::OpenOptions;
use std::io::Write;

/// Записывает PID и ждёт, пока наблюдатель завершит процесс.
fn main() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--test-profile") {
        assert_eq!(std::env::var("WORKSPACE_TEST_LOGIN_A").as_deref(), Ok("agent-fixture"));
        let directory = std::env::temp_dir();
        assert!(directory.file_name().unwrap().to_string_lossy().starts_with("workspace-dev-agent."));
        std::fs::write("agent-profile", directory.as_os_str().as_encoded_bytes())?;
    } else {
        assert!(std::env::current_exe()?.ends_with("dev-run/workspace-desktop"));
    }
    let mut launches = OpenOptions::new().create(true).append(true).open("launches")?;
    writeln!(launches, "{}", std::process::id())?;
    std::thread::sleep(std::time::Duration::from_secs(60));
    Ok(())
}
EOF
gallery_data="$fixture/crates/workspace-desktop/examples/gallery-label.txt"
printf 'before\n' > "$gallery_data"
cat > "$fixture/crates/workspace-desktop/examples/ui_gallery.rs" <<'EOF'
//! Показывает, какую версию примера собрал и запустил наблюдатель.

use std::fs::OpenOptions;
use std::io::Write;

/// Записывает данные сборки и PID, затем ждёт остановки наблюдателя.
fn main() -> std::io::Result<()> {
    assert!(std::env::current_exe()?.ends_with("target/debug/examples/ui_gallery"));
    assert!(std::env::var_os("WORKSPACE_TEST_LOGIN_A").is_none());
    std::fs::write("gallery-label", include_str!("gallery-label.txt"))?;
    let mut launches = OpenOptions::new().create(true).append(true).open("launches")?;
    writeln!(launches, "{}", std::process::id())?;
    std::thread::sleep(std::time::Duration::from_secs(60));
    Ok(())
}
EOF
cat > "$fixture/bin/codesign" <<'EOF'
#!/bin/sh
set -eu
case "$1" in
    --force)
        [ "$2" = --timestamp=none ] && [ "$3" = --identifier ]
        [ "$4" = com.exordos.workspace-desktop.dev ] && [ "$5" = --sign ]
        [ "$6" = '0123456789ABCDEF0123456789ABCDEF01234567' ]
        printf '%s\n' signed >> sign-operations
        ;;
    --verify) ;;
    *) exit 1 ;;
esac
EOF
cat > "$fixture/bin/security" <<'EOF'
#!/bin/sh
printf '%s\n' '  Valid identities only' \
    '  1) 0123456789ABCDEF0123456789ABCDEF01234567 "Apple Development: Fixture"' \
    '     1 valid identities found'
EOF
chmod +x "$fixture/bin/codesign" "$fixture/bin/security"
"$cargo_bin" generate-lockfile --offline --manifest-path "$fixture/Cargo.toml"

start_watcher() {
    (
        cd "$fixture"
        export PATH="$fixture/bin:$(dirname -- "$cargo_bin"):/usr/bin:/bin:/usr/sbin:/sbin"
        export CARGO_TARGET_DIR="$fixture/target"
        unset WORKSPACE_DEV_SIGN_IDENTITY WORKSPACE_DEV_SIGN_KEYCHAIN
        unset WORKSPACE_TEST_URL WORKSPACE_TEST_LOGIN_A WORKSPACE_TEST_PASSWORD_A
        exec "$cargo_bin" dev "$@"
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

start_watcher --normal-profile
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
start_watcher --normal-profile
wait_for_launch 4
[ "$(wc -l < "$fixture/sign-operations")" -eq 4 ]
kill -TERM "$watcher_pid"
wait "$watcher_pid"
watcher_pid=

# Запуск без аргументов берёт тестовое окружение из файла и обходится без codesign.
printf 'WORKSPACE_TEST_LOGIN_A=agent-fixture\n' > "$fixture/.env.test.local"
start_watcher
wait_for_launch 5
[ "$(wc -l < "$fixture/sign-operations")" -eq 4 ]
first_profile=$(cat "$fixture/agent-profile")
[ -d "$first_profile" ]
printf '// Вторая сборка режима агента.\n' > "$source_file"
wait_for_launch 6
last_profile=$(cat "$fixture/agent-profile")
[ "$first_profile" != "$last_profile" ]
[ ! -e "$first_profile" ]
kill -TERM "$watcher_pid"
wait "$watcher_pid"
watcher_pid=
[ ! -e "$last_profile" ]

# Галерея собирает пример без входа и подхватывает новые данные из того же каталога.
start_watcher --gallery
wait_for_launch 7
first_pid=$launched_pid
[ "$(cat "$fixture/gallery-label")" = before ]
printf 'after\n' > "$gallery_data.tmp"
mv "$gallery_data.tmp" "$gallery_data"
wait_for_launch 8
[ "$(cat "$fixture/gallery-label")" = after ]
[ "$(wc -l < "$fixture/sign-operations")" -eq 4 ]
if kill -0 "$first_pid" 2>/dev/null; then
    printf '%s\n' 'Первый процесс галереи остался запущен.' >&2
    exit 1
fi
kill -TERM "$watcher_pid"
wait "$watcher_pid"
watcher_pid=
if kill -0 "$launched_pid" 2>/dev/null; then
    printf '%s\n' 'Последний процесс галереи остался запущен.' >&2
    exit 1
fi
