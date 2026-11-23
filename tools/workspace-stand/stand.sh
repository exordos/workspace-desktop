#!/usr/bin/env bash
# Общий жизненный цикл стенда для всех checkout Workspace Desktop.
set -euo pipefail

tool_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
state_dir="${XDG_DATA_HOME:-$HOME/.local/share}/workspace-desktop/stand"
backend_origin=https://github.com/exordos/workspace_backend
port=21443
origin="https://127.0.0.1:$port"
umask 077

die() {
    printf '%s\n' "$*" >&2
    exit 1
}

usage() {
    cat <<'HELP'
cargo stand           Запустить стенд или переиспользовать работающий
cargo stand restart   Перезапустить текущую ревизию, сохранив данные
cargo stand update    Проверить origin/master и применить новую ревизию
cargo stand stop      Остановить сервисы, сохранив данные
cargo stand env A|B   Вывести окружение для тестового профиля Desktop
HELP
}

compose() {
    docker compose --env-file "$state_dir/runtime.env" -f "$state_dir/compose.yaml" "$@"
}

check_docker() {
    command -v docker >/dev/null || die "Для стенда нужен Docker Desktop или Docker Engine с Compose."
    docker compose version >/dev/null 2>&1 || die "Docker Compose недоступен. Установку нужно выполнить отдельно."
    docker info >/dev/null 2>&1 || die "Docker недоступен. Запусти Docker Desktop или проверь доступ к Docker Engine."
}

ready() {
    local running service
    running=$(compose ps --status running --services) || return
    for service in db s3 iam api events worker gateway; do
        grep -qx "$service" <<< "$running" || return 1
    done
    curl --silent --show-error --fail --max-time 3 --cacert "$state_dir/tls/ca.pem" \
        "$origin/api/core/v1/iam/clients/default/actions/jwks" >/dev/null || return
    compose exec -T iam python /stand/probe.py
}

wait_ready() {
    local deadline=$((SECONDS + 60))
    until ready >"$state_dir/readiness.log" 2>&1; do
        if (( SECONDS >= deadline )); then
            cat "$state_dir/readiness.log" >&2
            compose ps >&2
            die "Стенд не готов. Журналы сервисов: docker compose -f $state_dir/compose.yaml --env-file $state_dir/runtime.env logs"
        fi
        sleep 1
    done
}

report() {
    local revision
    revision=$(cat "$state_dir/backend.sha")
    printf 'Workspace Stand %s\nBackend %s\n' "$origin" "$revision"
}

create_keys() {
    local tls="$state_dir/tls"
    mkdir -p "$tls"
    if [[ ! -s "$state_dir/accounts.env" ]]; then
        local password_a password_b
        password_a=$(openssl rand -hex 18)
        password_b=$(openssl rand -hex 18)
        printf 'STAND_PASSWORD_A=%s\nSTAND_PASSWORD_B=%s\n' "$password_a" "$password_b" \
            > "$state_dir/accounts.env"
    fi
    if [[ ! -s "$state_dir/jwt.key" ]]; then
        openssl genrsa -out "$state_dir/jwt.key" 2048
    fi
    if [[ ! -s "$tls/ca.pem" || ! -s "$tls/ca.key" ]]; then
        openssl genrsa -out "$tls/ca.key" 2048
        cat > "$tls/ca.cnf" <<'CA'
[req]
distinguished_name = dn
x509_extensions = extensions
prompt = no
[dn]
CN = Workspace Desktop Local Stand CA
[extensions]
basicConstraints = critical,CA:TRUE,pathlen:0
keyUsage = critical,keyCertSign,cRLSign
CA
        openssl req -new -x509 -sha256 -days 3650 -config "$tls/ca.cnf" \
            -key "$tls/ca.key" -out "$tls/ca.pem"
        rm -f "$tls/server.pem"
    fi
    if [[ ! -s "$tls/server.key" || ! -s "$tls/server.pem" ]] ||
        ! openssl x509 -in "$tls/server.pem" -checkend 86400 -noout >/dev/null; then
        local serial
        serial=$(openssl rand -hex 16)
        openssl genrsa -out "$tls/server.key" 2048
        openssl req -new -key "$tls/server.key" -subj '/CN=localhost' -out "$tls/server.csr"
        cat > "$tls/server.cnf" <<'TLS'
basicConstraints = critical,CA:FALSE
keyUsage = critical,digitalSignature,keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = DNS:localhost,DNS:gateway,IP:127.0.0.1
TLS
        openssl x509 -req -sha256 -in "$tls/server.csr" -CA "$tls/ca.pem" \
            -CAkey "$tls/ca.key" -set_serial "0x$serial" -days 365 \
            -extfile "$tls/server.cnf" -out "$tls/server.pem"
    fi
}

backend_revision() {
    local cache="$state_dir/backend.git"
    if [[ ! -d "$cache" ]]; then
        git clone --quiet --bare --single-branch --branch master "$backend_origin" "$cache" >&2
    elif ! git --git-dir="$cache" fetch --quiet origin '+refs/heads/master:refs/heads/master'; then
        [[ "$action" != update ]] || die "Не удалось обновить workspace_backend из origin/master."
        printf '%s\n' "GitHub недоступен. Использую ревизию workspace_backend из локального кэша." >&2
    fi
    git --git-dir="$cache" rev-parse refs/heads/master
}

build_image() (
    local revision=$1 image=$2 source
    if docker image inspect "$image" >/dev/null 2>&1; then
        return
    fi
    source=$(mktemp -d "$state_dir/source.XXXXXXXX")
    trap 'rm -rf "$source"' EXIT
    git --git-dir="$state_dir/backend.git" archive "$revision" | tar -xf - -C "$source"
    docker build -f "$tool_dir/Dockerfile" -t "$image" "$source"
)

install_bundle() {
    local stopped=0
    if [[ -f "$state_dir/runtime.env" ]]; then
        compose stop gateway api events worker iam
        stopped=1
    fi
    mkdir -p "$state_dir/bundle"
    cp "$tool_dir/compose.yaml" "$state_dir/compose.yaml"
    cp "$tool_dir/"{backend.conf,nginx.conf,identities.py,iam.py,init.py,probe.py} "$state_dir/bundle/"
    rm -f "$state_dir/backend.sha"
    printf 'WORKSPACE_STAND_IMAGE=%s\nWORKSPACE_STAND_MINIO_IMAGE=%s\nWORKSPACE_STAND_PORT=%s\n' \
        "$image" "$minio_image" "$port" \
        > "$state_dir/runtime.env"
    if (( stopped )); then
        compose rm -f gateway api events worker iam
    fi
    compose up -d --wait --wait-timeout 60 db s3
    printf '%s\n' "Применяю миграции PostgreSQL."
    if ! compose run --rm --no-deps migrate >"$state_dir/migrations.log" 2>&1; then
        tail -40 "$state_dir/migrations.log" >&2
        die "Миграции PostgreSQL не завершились. Журнал: $state_dir/migrations.log"
    fi
    compose run --rm --no-deps api python /stand/init.py
    compose up -d iam api events worker gateway
    wait_ready
    compose exec -T iam python /stand/probe.py prepare
    printf '%s\n' "$revision" > "$state_dir/backend.sha"
}

print_env() {
    local account=$1 password login
    case "$account" in
        A) login=stand-a ;;
        B) login=stand-b ;;
        *) die "Укажи аккаунт A или B." ;;
    esac
    [[ -s "$state_dir/backend.sha" ]] || die "Сначала запусти cargo stand."
    password=$(sed -n "s/^STAND_PASSWORD_$account=//p" "$state_dir/accounts.env")
    [[ -n "$password" ]] || die "Не найдены данные тестового аккаунта."
    printf 'export WORKSPACE_TEST_URL=%q\n' "$origin"
    printf 'export WORKSPACE_TEST_LOGIN_A=%q\n' "$login"
    printf 'export WORKSPACE_TEST_PASSWORD_A=%q\n' "$password"
    printf 'unset WORKSPACE_TEST_PROJECT_ID\n'
    printf 'export SSL_CERT_FILE=%q\n' "$state_dir/tls/ca.pem"
}

action=${1:-start}
case "$action" in
    -h|--help|help) usage; exit ;;
    env) [[ $# == 2 ]] || die "Используй cargo stand env A или cargo stand env B."
         print_env "$2"; exit ;;
    start|restart|stop|update) [[ $# -le 1 ]] || die "Лишние аргументы. См. cargo stand --help." ;;
    *) die "Неизвестная команда. См. cargo stand --help." ;;
esac
check_docker
case "$action" in
    stop)
        if [[ -f "$state_dir/runtime.env" ]]; then
            compose stop
        fi
        exit ;;
    restart)
        [[ -s "$state_dir/backend.sha" ]] || die "Сначала запусти cargo stand."
        create_keys
        compose stop gateway api events worker iam
        compose restart db s3
        compose up -d --wait --wait-timeout 60 db s3
        compose up -d iam api events worker gateway
        wait_ready
        report
        exit ;;
    start)
        if [[ -s "$state_dir/backend.sha" ]] && ready >/dev/null 2>&1; then
            report
            exit
        fi ;;
esac
mkdir -p "$state_dir"
revision=$(backend_revision)
recipe=$(openssl dgst -sha256 -r "$tool_dir/Dockerfile" | cut -c1-12)
image="workspace-desktop-stand-backend:$revision-$recipe"
minio_recipe=$(openssl dgst -sha256 -r "$tool_dir/Dockerfile.minio" | cut -c1-12)
minio_image="workspace-desktop-stand-minio:$minio_recipe"
if [[ "$action" == update && -s "$state_dir/backend.sha" ]] &&
    grep -Fxq "WORKSPACE_STAND_IMAGE=$image" "$state_dir/runtime.env" &&
    grep -Fxq "WORKSPACE_STAND_MINIO_IMAGE=$minio_image" "$state_dir/runtime.env" &&
    cmp -s "$tool_dir/compose.yaml" "$state_dir/compose.yaml"; then
    unchanged=1
    for file in backend.conf nginx.conf identities.py iam.py init.py probe.py; do
        cmp -s "$tool_dir/$file" "$state_dir/bundle/$file" || unchanged=0
    done
    if (( unchanged )) && ready >/dev/null 2>&1; then
        report
        exit
    fi
fi
build_image "$revision" "$image" >&2
if ! docker image inspect "$minio_image" >/dev/null 2>&1; then
    docker build -f "$tool_dir/Dockerfile.minio" -t "$minio_image" "$tool_dir" >&2
fi
create_keys
install_bundle >&2
report
