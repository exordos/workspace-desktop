#!/bin/sh
set -eu

# Сборка остаётся в общем target. Подписываем только отдельную копию для cargo dev.
target_dir=${CARGO_TARGET_DIR:-${CARGO_BUILD_TARGET_DIR:-target}}
cargo build --target-dir "$target_dir" -p workspace-desktop --bin workspace-desktop
binary="$target_dir/debug/workspace-desktop"

if [ "${1-}" = --test-profile ]; then
    if [ -z "${WORKSPACE_TEST_URL-}${WORKSPACE_TEST_LOGIN_A-}${WORKSPACE_TEST_PASSWORD_A-}" ]; then
        # В отдельном worktree .env.test.local может быть только в основном checkout.
        credentials=./.env.test.local
        if [ ! -r "$credentials" ]; then
            git_dir=$(git rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)
            if [ -n "$git_dir" ]; then
                credentials="$(dirname -- "$git_dir")/.env.test.local"
            fi
        fi
        if [ -r "$credentials" ]; then
            set -a
            . "$credentials"
            set +a
        fi
    fi
    # Наблюдатель прерывает приложение при пересборке. Сценарий запуска убирает весь профиль.
    run_dir=$(mktemp -d "${TMPDIR:-/tmp}/workspace-dev-agent.XXXXXX")
    client=
    trap 'exit 143' TERM
    trap 'exit 130' INT
    trap '
        if [ -n "$client" ]; then kill -TERM "$client" 2>/dev/null || true; wait "$client" || true; fi
        rm -rf -- "$run_dir"
    ' EXIT
    TMPDIR="$run_dir" "$binary" --test-profile &
    client=$!
    wait "$client"
    client=
    exit 0
fi

identity=${WORKSPACE_DEV_SIGN_IDENTITY-}
if [ -z "$identity" ]; then
    # Если отдельный сертификат не настроен, используем единственный Apple Development.
    set --
    if [ -n "${WORKSPACE_DEV_SIGN_KEYCHAIN-}" ]; then
        set -- "$WORKSPACE_DEV_SIGN_KEYCHAIN"
    fi
    identity=$(security find-identity -p codesigning "$@" | awk '
        /Valid identities only/ { valid = 1 }
        !valid && /"Workspace Desktop Local Development"/ { local_cert = $2; local_count++ }
        valid && /"Apple Development:/ { apple_cert = $2; apple_count++ }
        END {
            if (local_count == 1) print local_cert
            else if (!local_count && apple_count == 1) print apple_cert
        }
    ')
    identity=${identity:-Workspace Desktop Local Development}
fi
if [ "$identity" = - ]; then
    printf '%s\n' 'Для cargo dev --normal-profile нужен постоянный сертификат подписи.' >&2
    exit 1
fi
run_dir="$target_dir/dev-run"
mkdir -p "$run_dir"
staging=$(mktemp -d "$run_dir/.sign.XXXXXX")
trap 'rm -rf -- "$staging"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
signed="$staging/workspace-desktop"
cp "$binary" "$signed"
set -- --force --timestamp=none --identifier com.exordos.workspace-desktop.dev --sign "$identity"
if [ -n "${WORKSPACE_DEV_SIGN_KEYCHAIN-}" ]; then
    set -- "$@" --keychain "$WORKSPACE_DEV_SIGN_KEYCHAIN"
fi
if ! codesign "$@" "$signed"; then
    printf '%s\n' 'Настройте сертификат для cargo dev по разделу «Dev-запуск на macOS» в docs/plans/modules/auth.md.' >&2
    exit 1
fi
codesign --verify "$signed"
mv -f "$signed" "$run_dir/workspace-desktop"
rmdir "$staging"
trap - EXIT INT TERM
exec "$run_dir/workspace-desktop"
