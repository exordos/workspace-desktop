#!/bin/sh
set -eu
umask 077

if [ "$#" -ne 1 ] || [ -z "$1" ]; then
    printf '%s\n' 'Использование: sh scripts/check-workspace-desktop-ui.sh <новый-каталог-результатов>' >&2
    exit 2
fi
if [ "$(uname -s)" != Darwin ]; then
    printf '%s\n' 'Для проверки GPUI требуется macOS.' >&2
    exit 1
fi

case $1 in
    /*) output=$1 ;;
    *) output=$PWD/$1 ;;
esac
project_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$project_dir"
sha=$(git rev-parse HEAD)
status=$(git status --porcelain)
if [ -n "$status" ]; then
    printf '%s\n' 'Для проверки нужен чистый worktree.' >&2
    exit 1
fi
mkdir "$output"

CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-"$output/target"}
CARGO_BUILD_BUILD_DIR=$CARGO_TARGET_DIR
export CARGO_TARGET_DIR CARGO_BUILD_BUILD_DIR

# Ищем точную строку один раз. Cargo считает запуск с нулём тестов успешным.
has_one_line() {
    awk -v expected="$1" '$0 == expected { count++ } END { exit(count != 1) }' "$2"
}

fail() {
    printf 'Ошибка проверки %s. Журнал: %s\n' "$1" "$2" >&2
    exit 1
}

if ! {
    printf 'sha=%s\n' "$sha" &&
        printf 'cargo_target_dir=%s\n' "$CARGO_TARGET_DIR" &&
        uname -m &&
        sw_vers &&
        rustc --version &&
        cargo --version
} >"$output/environment.log" 2>&1; then
    fail 'окружения' "$output/environment.log"
fi

if ! cargo tree --locked -p workspace-desktop -i gpui >"$output/gpui-version.log" 2>&1; then
    fail 'версии GPUI' "$output/gpui-version.log"
fi

run_lib_tests() {
    cargo test --locked -p workspace-desktop --features gpui-test --lib "$@"
}

if ! run_lib_tests -- --list >"$output/tests-list.log" 2>&1; then
    fail 'списка тестов' "$output/tests-list.log"
fi

# Полные имена исключают совпадения с другими тестами при изменении набора.
set -- \
    gpui_client::client::loopback::catalog_click_and_submission_reach_https_queue \
    gpui_client::client::loopback::explicit_topic_accepts_mouse_and_keyboard_in_each_account \
    gpui_client::client::loopback::explicit_topic_is_rechecked_before_queue_and_before_post \
    gpui_client::client::loopback::late_history_keeps_the_reopened_account_conversation \
    gpui_client::client::loopback::local_wss_recovers_pruned_cursor_and_changed_epoch \
    gpui_client::conversation::send_tests::stability::history_keys_scroll_and_load_older_page \
    gpui_client::conversation::send_tests::editor_keyboard_and_ime_share_the_same_send_path \
    gpui_client::platform_window::tests::gpui_tests::notification_click_reaches_its_account_and_message_in_the_same_window \
    gpui_client::runtime::tests::shutdown_waits_for_started_work_and_releases_profile

for test_name do
    if ! has_one_line "$test_name: test" "$output/tests-list.log"; then
        fail "регистрации $test_name" "$output/tests-list.log"
    fi
done

for test_name do
    log="$output/${test_name##*::}.log"
    if ! run_lib_tests "$test_name" -- --exact --format pretty --color never >"$log" 2>&1; then
        fail "$test_name" "$log"
    fi
    if ! has_one_line "test $test_name ... ok" "$log"; then
        fail "выполнения $test_name" "$log"
    fi
    printf 'Проверен %s\n' "$test_name"
done

actual_sha=$(git rev-parse HEAD)
status=$(git status --porcelain)
if [ "$actual_sha" != "$sha" ] || [ -n "$status" ]; then
    printf 'Worktree изменился во время прогона. Журналы: %s\n' "$output" >&2
    exit 1
fi
printf 'Проверок пройдено: %s. Журналы: %s\n' "$#" "$output"
