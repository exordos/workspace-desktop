# Сборки Workspace Desktop в GitHub Actions

## Первый этап

[Workflow CI](../../../.github/workflows/ci.yml) запускается при push в `CI` и `main`, при pull request в `main` и вручную после появления workflow в основной ветке. Он выполняет `cargo check-architecture` в отдельном задании. Сборки и тесты используют Rust stable и версии зависимостей из `Cargo.lock`.

Проверку форматирования можно включить после отдельного прогона `cargo fmt` по репозиторию. На исходном `main` проверка форматирования завершалась с ошибкой.

| ОС | x86-64 | ARM64 | Что проверяем |
| --- | --- | --- | --- |
| macOS | `macos-15-intel` | `macos-15` | Release-сборка приложения, проверка архитектуры и подписи пакета, запуск бинарника с `--help`. |
| Linux | `ubuntu-24.04` | `ubuntu-24.04-arm` | Сборка и запуск `cargo test --locked --workspace --no-default-features` на каждой архитектуре. |
| Windows | `windows-2025` | `windows-11-arm` | Сборка и запуск `cargo test --locked --workspace --no-default-features` на каждой архитектуре. |

В успешном прогоне на странице Actions доступны два ZIP с `Workspace Desktop.app`. Скрипт упаковки использует ad hoc подпись. Это сборки для проверки, без подписи Developer ID и нотариализации. Срок хранения — 14 дней. Команда `--help` проверяет запуск процесса. Проверка окна, входа и системных уведомлений описана в [критериях выпуска](gpui-ui-parity-release.md).

В [Workspace UI](https://github.com/exordos/workspace_ui/blob/d2935e8a42cfc3d6c29527eb4da1f6274d604c89/.github/workflows/ci.yml) проверки отделены от сборок, а артефакты хранятся 14 дней. Конфигурация Electron задаёт обе архитектуры на трёх ОС. В опубликованном релизе `0.9.13` пакет Linux есть для x86-64. [Цели Electron](https://github.com/exordos/workspace_ui/blob/d2935e8a42cfc3d6c29527eb4da1f6274d604c89/packages/electron/electron-builder.yml) · [Релиз](https://github.com/exordos/workspace_ui/releases/tag/0.9.13)

В Workspace Native сборки macOS и Windows идут на отдельных нативных раннерах для каждой архитектуры. Сборка Linux использует Ubuntu 24.04 x86-64. [macOS и Windows](https://github.com/exordos/workspace_native/blob/4c21dafbe7424ab313c9dfb61d8c128b118feb14/.github/workflows/desktop-platform-packages.yml) · [Linux](https://github.com/exordos/workspace_native/blob/4c21dafbe7424ab313c9dfb61d8c128b118feb14/.github/workflows/linux-release.yml)

## Приложение для Linux и Windows

Сейчас оконный клиент GPUI подключён только на macOS. Бинарник клиента на Linux и Windows завершается с сообщением о требовании macOS, а команды `workspace-auth` зависят от Keychain. Поэтому на этих ОС CI проверяет переносимый код. Чтобы получить рабочие пакеты, потребуются следующие этапы.

1. Подключить общий клиент GPUI к оконным бэкендам Linux и Windows. Вынести вызовы AppKit из общего UI в платформенные адаптеры и добиться открытия окна на обеих системах. [Границы модулей](../workspace-desktop-architecture.md#модули-и-ответственность)
2. Подключить каталоги профиля, системное защищённое хранилище, буфер обмена, уведомления и управление окном. На каждой ОС проверить вход, восстановление сессии, открытие чата и отправку сообщения. [Авторизация](../modules/auth.md) · [Оболочка и платформа](../modules/shell-platform.md)
3. Перевести текущие задания Linux и Windows на сборку и упаковку клиента для обеих архитектур. Перед публикацией каждого артефакта проверить архитектуру и запуск пакета на целевой ОС.
