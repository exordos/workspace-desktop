# Проверки выпуска и обновления GPUI

Текущий ориентир — Workspace Desktop на GPUI `0.2.2`. [UI-17](gpui-ui-parity.md#ui-17-проверки-для-выпуска-и-обновления-gpui) · [План реализации](ui-parity/ui-17.md) · [Приёмка базового мессенджера](gpui-ui-parity-acceptance.md).

## Короткий прогон

Из чистого worktree на macOS:

```sh
out="$(mktemp -d "${TMPDIR:-/tmp}/workspace-ui.XXXXXXXX")"
sh scripts/check-workspace-desktop-ui.sh "$out/smoke"
```

Команда проверяет SHA и чистоту исходников до и после прогона, записывает окружение в `environment.log`, фактическую версию зависимости в `gpui-version.log` и полный список тестов в `tests-list.log`. Журнал каждого теста называется по его короткому имени. Запуск завершается ошибкой, если тест не зарегистрирован ровно один раз, команда теста упала или в журнале отсутствует точный результат `ok`.

По умолчанию сборка находится в `$out/smoke/target`. Для повторных прогонов можно передать отдельный абсолютный `CARGO_TARGET_DIR` для текущего worktree. Команда размещает там и `CARGO_BUILD_BUILD_DIR`. С подготовленным Cargo-кэшем добавьте `CARGO_NET_OFFLINE=true`. Используется lockfile. Путь результатов должен быть новым и находиться вне отслеживаемых исходников.

| Что может сломаться | Тест и наблюдаемый результат |
| --- | --- |
| Клавиатура теряет маршрут, отправка теряется или дублируется | `catalog_click_and_submission_reach_https_queue`. Tab до истории и редактора и обратно. После отказа записи ввод сохраняется. Повтор отправляет один POST и подтверждает сообщение в SQLite. |
| Выбирается неверная тема или пропускается проверка доступа | `explicit_topic_accepts_mouse_and_keyboard_in_each_account` открывает точную тему мышью и клавиатурой для двух аккаунтов. `explicit_topic_is_rechecked_before_queue_and_before_post` останавливает закрытую, зашифрованную или чужую цель. |
| Запоздавшая история меняет маршрут или WSS не восстанавливается | `late_history_keeps_the_reopened_account_conversation` удерживает выбранный экран после A → B → A. `local_wss_recovers_pruned_cursor_and_changed_epoch` проверяет восстановление снимка и курсора при истечении курсора и смене эпохи. |
| Ломаются клавиши истории или редактора | `history_keys_scroll_and_load_older_page` проверяет прокрутку и догрузку. `editor_keyboard_and_ime_share_the_same_send_path` покрывает подтверждение текста, вставку и путь отправки с клавиатуры. |
| Клик по уведомлению открывает неверный аккаунт или окно | `notification_click_reaches_its_account_and_message_in_the_same_window` доставляет синтетический клик в тот же клиент, открывает B и точное сообщение. |
| Выход теряет начатую работу или блокирует перезапуск | `shutdown_waits_for_started_work_and_releases_profile` проверяет ожидание начатой работы и освобождение профиля. |

Для диагностики повторите один тест через Cargo с выводом его внутренних сообщений:

```sh
cargo test --locked -p workspace-desktop --features gpui-test --lib \
  catalog_click_and_submission_reach_https_queue -- --nocapture
```

Короткий прогон использует синтетические GPUI-окна и локальный HTTPS/WSS. Он не требует Keychain, системных разрешений, тестового аккаунта или сетевого стенда.

## Кандидат к выпуску

### Сборки и проверки без нативного окна

Из отдельного чистого worktree. Используйте его собственные каталоги сборки. Перед запуском сохраните вывод этих команд под SHA кандидата и отметьте команды, завершившиеся ошибкой. Короткий прогон повторите в том же окружении.

```sh
out="$(mktemp -d "${TMPDIR:-/tmp}/workspace-release.XXXXXXXX")"
export CARGO_TARGET_DIR="$out/target"
export CARGO_BUILD_BUILD_DIR="$CARGO_TARGET_DIR"
set -e

sh scripts/check-workspace-desktop-ui.sh "$out/smoke"
cargo fmt --all -- --check >"$out/fmt.log" 2>&1
cargo clippy --locked --workspace --all-targets -- -D warnings >"$out/clippy.log" 2>&1
cargo clippy --locked -p workspace-desktop --features gpui-test --all-targets -- -D warnings \
  >"$out/clippy-gpui.log" 2>&1
cargo test --locked --workspace >"$out/workspace.log" 2>&1
cargo test --locked --workspace --no-default-features >"$out/headless.log" 2>&1
cargo test --locked -p workspace-desktop --features gpui-test --all-targets >"$out/gpui.log" 2>&1
cargo build --locked -p workspace-desktop --bin workspace-desktop \
  --example ui_gallery --example gpui_chat_probe --example gpui_macos_window_probe --example ui_scene \
  >"$out/package-build.log" 2>&1
cargo build --locked -p workspace-desktop --no-default-features --bin workspace-auth \
  >"$out/cli-build.log" 2>&1
"$CARGO_TARGET_DIR/debug/examples/ui_scene" --list >"$out/scenes.log"
cargo tree --locked --workspace -e no-dev >"$out/dependencies.log"
```

В списке сцен ожидается `chat-row`, в дереве рабочих зависимостей — отсутствие Iced. Команды показывают обычный Cargo-профиль, использованный в GPUI-21. Если публикуемый пакет собирается с `--release`, соберите и упакуйте бинарник из каталога `release`. Дальнейшие проверки окна и стенда относятся к выпускаемому пакету.

Проверьте headless CLI с отдельным пустым профилем. В `--help` ожидается `sync`, `status` завершается успешно, а `sync` возвращает ненулевой статус с той же причиной. Оба вывода должны содержать строку `No saved Workspace accounts.`

```sh
cli="$CARGO_TARGET_DIR/debug/workspace-auth"
"$cli" --help >"$out/cli-help.log"
grep -q 'sync' "$out/cli-help.log"
mkdir "$out/cli-empty-profile"
WORKSPACE_AUTH_STATE_DIR="$out/cli-empty-profile" "$cli" status >"$out/cli-status.log"
if WORKSPACE_AUTH_STATE_DIR="$out/cli-empty-profile" "$cli" sync >"$out/cli-sync.log" 2>&1; then
    printf '%s\n' 'Ожидалась ошибка отсутствия аккаунтов.' >&2
    exit 1
fi
grep -Fx 'No saved Workspace accounts.' "$out/cli-status.log"
grep -Fx 'No saved Workspace accounts.' "$out/cli-sync.log"
```

Для debug-пакета:

```sh
cp "$CARGO_TARGET_DIR/debug/workspace-desktop" "$out/workspace-desktop"
app="$(sh scripts/bundle-workspace-desktop-macos.sh "$out/workspace-desktop" 2>"$out/bundle.log")"
codesign --verify --strict "$app" >"$out/codesign.log" 2>&1
file "$app/Contents/MacOS/workspace-desktop" >"$out/binary.log"
shasum -a 256 "$app/Contents/MacOS/workspace-desktop" "$app/Contents/Info.plist" >"$out/package.sha256"
test "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$app/Contents/Info.plist")" = com.exordos.workspace-desktop
test "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$app/Contents/Info.plist")" = workspace-desktop
```

Сверьте архитектуру в `binary.log` с целевой. Скрипт создаёт локальную ad hoc подпись. При выпуске с иной подписью повторите проверку с итоговым экземпляром. SHA исходников и версия GPUI лежат в журналах короткого прогона.

### Нативное окно и сеть

В интерактивной сессии macOS запускайте по одному экземпляру. Перейдите к следующему после завершения предыдущего.

1. **Нативные пробы на синтетике.** Запустите `cargo run --locked -p workspace-desktop --example gpui_macos_window_probe`. Нажмите пробел, скройте окно красной кнопкой и верните из Workspace в строке меню. Проверьте тот же ID окна и новый прирост счётчика пробелом. Повторите с `WORKSPACE_DESKTOP_DISABLE_TRAY=1` и проверьте выход по красной кнопке. Для прокрутки используйте `cargo run --locked -p workspace-desktop --example gpui_chat_probe -- --reproduce-scroll-jump`. Журнал связывает опорную строку и смещение по `op_id`. [Инструкция по прокрутке](gpui-probe.md#воспроизвести-скачок-истории-при-догрузке-сверху).
2. **Штатный пакет с пустым профилем.** Создайте `mkdir "$out/app-empty-profile"` и выполните `open --env "WORKSPACE_AUTH_STATE_DIR=$out/app-empty-profile" --env WORKSPACE_DESKTOP_DEBUG=1 --stderr "$out/app.log" "$app"`. Проверьте скрытие, восстановление того же окна по номеру и явный выход, дождитесь окончания процесса и `event=workspace_shutdown_finished result=clean`. Запишите путь к пакету, PID, номер окна и наблюдения. Пустой профиль позволяет проверить окно, меню и выход без Keychain.
3. **Обмен на выделенном тестовом Workspace.** На выпускаемом пакете с изолированным профилем A/B выполните шаги [баннера](../modules/shell-platform.md#клик-по-баннеру-macos) и [жизненного цикла окна](../modules/shell-platform.md#жизненный-цикл-окна-из-строки-меню-macos). Дождитесь роста счётчика B, щёлкните по настоящему баннеру из открытого и свёрнутого состояния. Проверьте фокус и точное сообщение. В том же окне пройдите [клавиатурный маршрут](gpui-ui-parity-accessibility.md#маршрут-с-клавиатуры), включая Escape, Page Up/End, вставку и перенос строки. На одном тестовом разговоре отправьте ответ, со второго клиента подтвердите одну доставку. При восстановлении сети проверьте сходимость истории, сводок и курсора без повторов, затем завершите процесс. Записывайте наблюдения, псевдонимы A/B, технические ID и `op_id`. Для этого шага требуются тестовый профиль, Keychain, системные уведомления и доступ к стенду. При блокере оставьте соответствующий результат открытым и назовите недоступный шаг.

Для текущих проверок клавиатуры, ввода и переключения аккаунта без стенда есть [синтетические loopback-тесты UI-12](gpui-ui-parity-acceptance.md#воспроизведение). Исторический нативный сценарий UI-12 также подтвердил это в одном окне на `1710e63`, но использовал внешний `ui-12/native-accepted.patch` и `ui-10/read-ax.swift` из каталога журнала. При наличии артефактов сверьте применимость патча в отдельном worktree, соберите AX-помощник `swiftc`, передайте путь в `WORKSPACE_UI10_AX_HELPER` и повторите `cargo run --locked -p workspace-desktop --example ui_scene -- --ui05-native <новая-папка>`. Для точного повтора нужны уже предоставленные системные разрешения. Ожидаются `result.json` со статусами `Ok/Clean` и `ui12-route.evidence.json`. Исторический результат относится к синтетическому Client, обмен на нём покрыт отдельным loopback-прогоном.

## Сравнение при смене GPUI

1. Снимите базу на принятом SHA до изменения зависимости. В новом worktree измените закреплённый GPUI в `crates/workspace-desktop/Cargo.toml`, обновите `Cargo.lock` и проверьте все изменённые зависимости. Для отдельной Storybook-галереи, если она входит в обновление, проверьте её собственные Cargo.toml и Cargo.lock. [UI-03](gpui-ui-parity-storybook.md).
2. Для каждого SHA повторите короткий прогон, сборки и нативные проверки по этому документу. Используйте одинаковые версии Rust и macOS, Cargo features, профиль сборки, данные, язык, тему, дисплей и масштаб. Сравните рабочее окно на 1440 и 700 логических px. Если система ограничивает высоту, запишите фактический размер. Проверьте фокус шапки, каталога, истории, редактора, ввод после сворачивания и позицию истории при догрузке. У GPUI 0.2.2 видимая клавиатурная рамка доступна, AX показывает системную рамку и кнопки AppKit, внутренние контролы недоступны VoiceOver, масштабирование содержимого отсутствует. Если AX-помощник с выданным доступом отсутствует, отметьте, что AX отдельно не проверялся. [UI-10](gpui-ui-parity-accessibility.md).
3. Запустите два независимых [замера UI-11](gpui-ui-parity-stability.md#запуск-и-замеры) для каждого SHA. Сравните UUID и координаты опоры, зазор у хвоста, последовательность 64 клавиш и пяти переносов, CPU-подготовку кадра и пиковый RSS. Допуск для опоры — 1 логический px, хвост — 0 px после вычета внутреннего поля. Для CPU и RSS сравните разброс повторов и перепроверьте заметные отклонения. Эти замеры не включают время показа кадра на экране и работу GPU.
4. При наличии сопоставимых кадров просмотрите визуальный diff между версиями вручную. При ранее предоставленном Screen Recording запустите существующую сцену `cargo run --locked -p workspace-desktop --example ui_scene -- chat-row light 700x600 "$out/scene"` в новый каталог. Сверьте `run.json` с темой, фактическим размером, масштабом и границами строки. `--verify` существующей сцены добавляет `diff.json` и `diff.png` для повтора внутри одного запуска и намеренного сдвига. Если захват недоступен, запишите причину и выполните ручную проверку окна.

В сводке для каждого существенного отличия укажите пару SHA, сценарий, фактический результат, путь к журналу или кадру и оценку регрессии. При скачке истории запишите `op_id`, UUID и координаты опоры, число строк и поправку. [Диагностические журналы](../workspace-desktop-architecture.md#диагностические-журналы).
