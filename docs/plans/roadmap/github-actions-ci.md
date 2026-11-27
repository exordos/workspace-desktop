# Сборки Workspace Desktop в GitHub Actions

## Первый этап

[Workflow CI](../../../.github/workflows/ci.yml) запускается при push в `main`, при pull request в `main`, при push тега версии и вручную. Он выполняет `cargo check-architecture` в отдельном задании. Сборки и тесты используют Rust stable и версии зависимостей из `Cargo.lock`.

Проверку форматирования можно включить после отдельного прогона `cargo fmt` по репозиторию. На исходном `main` проверка форматирования завершалась с ошибкой.

| ОС | x86-64 | ARM64 | Что проверяем |
| --- | --- | --- | --- |
| macOS | `macos-15-intel` | `macos-15` | Release-сборка приложения, проверка архитектуры и подписи пакета, запуск бинарника с `--help`. |
| Linux | `ubuntu-24.04` | — | Переносимые тесты, release-сборка приложения и `tar.gz`. |
| Windows | `windows-2025` | — | Release-сборка приложения и ZIP. |

В успешном прогоне на странице Actions доступны два ZIP с `Workspace Desktop.app`, ZIP с `workspace-desktop.exe` и архив `workspace-desktop-linux-x64.tar.gz`. Скрипт упаковки macOS использует ad hoc подпись. Срок хранения — 14 дней. Команда `--help` проверяет запуск процесса. Проверка окна, входа и системных уведомлений описана в [критериях выпуска](gpui-ui-parity-release.md).

В [Workspace UI](https://github.com/exordos/workspace_ui/blob/d2935e8a42cfc3d6c29527eb4da1f6274d604c89/.github/workflows/ci.yml) проверки отделены от сборок, а артефакты хранятся 14 дней. Конфигурация Electron задаёт обе архитектуры на трёх ОС. В опубликованном релизе `0.9.13` пакет Linux есть для x86-64. [Цели Electron](https://github.com/exordos/workspace_ui/blob/d2935e8a42cfc3d6c29527eb4da1f6274d604c89/packages/electron/electron-builder.yml) · [Релиз](https://github.com/exordos/workspace_ui/releases/tag/0.9.13)

В Workspace Native сборки macOS и Windows идут на отдельных нативных раннерах для каждой архитектуры. Сборка Linux использует Ubuntu 24.04 x86-64. [macOS и Windows](https://github.com/exordos/workspace_native/blob/4c21dafbe7424ab313c9dfb61d8c128b118feb14/.github/workflows/desktop-platform-packages.yml) · [Linux](https://github.com/exordos/workspace_native/blob/4c21dafbe7424ab313c9dfb61d8c128b118feb14/.github/workflows/linux-release.yml)

## Приложение для Linux и Windows

Windows и Linux используют общий оконный клиент GPUI. CI запускает переносимые тесты на Ubuntu x64 с `--no-default-features`. Утилита `workspace-auth` пока использует Keychain macOS.

### Локальный запуск на Windows x64

Первая точка приёмки — штатный `workspace-desktop.exe` на Windows 11 x64. Команда из корня репозитория собирает и запускает клиент. Изолированный пустой профиль показывает форму входа. После закрытия окна процесс завершается, а тот же профиль можно открыть снова.

```powershell
$env:WORKSPACE_AUTH_STATE_DIR = Join-Path ([System.IO.Path]::GetTempPath()) ('workspace-desktop-' + [guid]::NewGuid().ToString())
cargo run --locked -p workspace-desktop --bin workspace-desktop
```

Для сборки нужны Rust stable для `x86_64-pc-windows-msvc`, Visual Studio Build Tools с компонентами C++ и Windows SDK. Запускаем команду из Developer PowerShell для x64. Для `cargo build --release` GPUI 0.2.2 дополнительно вызывает `fxc.exe` из SDK. Если GPUI не находит его автоматически, передаём полный путь через `GPUI_FXC_PATH`. [Настройка Rust в Windows](https://learn.microsoft.com/en-us/windows/dev-environment/rust/setup) · [Скрипт сборки GPUI 0.2.2](https://docs.rs/crate/gpui/0.2.2/source/build.rs)

Обычный профиль Windows хранится в `%LOCALAPPDATA%/Workspace Desktop/auth`. Токены и ключи чатов сохраняются в файлах профиля, зашифрованных DPAPI для текущего пользователя. Приёмка после первого открытия окна включает вход с сохранением сессии и повторный запуск. [Границы модулей](../workspace-desktop-architecture.md#модули-и-ответственность) · [Авторизация](../modules/auth.md)

### Локальный запуск на Linux x64

Архив для первого предварительного выпуска собирается на Ubuntu 24.04 x64 с бэкендами X11 и Wayland. Для входа нужен Secret Service в графической сессии. Профиль хранится в `$XDG_DATA_HOME/workspace-desktop/auth`, а при отсутствии абсолютного `XDG_DATA_HOME` — в `$HOME/.local/share/workspace-desktop/auth`. В первый предварительный выпуск входят проверки сборки, распаковки и запуска процесса в CI. Проверки окна, входа, повторного запуска и отправки сообщения выполняются в графической сессии. [Критерии выпуска](gpui-ui-parity-release.md#кандидат-к-выпуску) · [Возможности GPUI 0.2.2](https://docs.rs/crate/gpui/0.2.2/features)

### Артефакты CI и релиз

Первые архивы приложения собираются нативно для x64. Сборка использует `cargo build --locked --release -p workspace-desktop --bin workspace-desktop`. [Скрипт упаковки Linux](../../../scripts/bundle-workspace-desktop-linux.sh) принимает готовый бинарник и выполняет те же проверки локально и в Actions.

- Перед загрузкой архива в Actions проверяем архитектуру и запускаем распакованный бинарник с `--help`.
- Для предварительных сборок Windows и Linux ещё нужны нативные уведомления, трей, вставка файлов из буфера, предпросмотр HEIF и воспроизведение видео. Сборки для ARM64 остаются отдельным этапом.

### Публикация в GitHub Releases

После успешного прогона на `main` поставьте тег новой версии на проверенный коммит. Подготовьте описание выпуска в Markdown и укажите файл при создании аннотированного тега.

```sh
git tag -a v0.1.0 -F release-notes.md <проверенный-SHA>
git push origin refs/tags/v0.1.0
```

Для предварительного выпуска используйте суффикс, например `v0.1.0-rc.1`. Для легковесного тега GitHub сгенерирует заметки по истории изменений.

В прогоне по новому тегу GitHub собирает и проверяет все четыре пакета. Затем публикует один выпуск `Workspace Desktop v…` с описанием и четырьмя архивами в Assets. Архивы остаются доступны после истечения срока хранения артефактов Actions. [Выпуски Workspace Desktop](https://github.com/exordos/workspace-desktop/releases)
