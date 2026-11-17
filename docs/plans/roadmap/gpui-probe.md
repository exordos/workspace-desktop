# Прототип GPUI на macOS

[План миграции](gpui-migration.md) · [GPUI 0.2.2](https://docs.rs/gpui/0.2.2/gpui/)

`workspace-desktop` использует точную версию GPUI `0.2.2` из crates.io.
Для сборки на macOS нужны Xcode с macOS SDK и Clang: GPUI генерирует
привязки к системным заголовкам. Feature `font-kit` включает системный
движок текста macOS. Прототип использует Metal и компилирует
шейдеры при открытии окна через `runtime_shaders`. Отдельный компилятор
`xcrun metal` для этого режима не нужен. Среда проверки: arm64,
macOS 27.0.1, Xcode 26.4.1 (macOS SDK 26.4), Rust 1.100.0.

Из корня репозитория в графической сессии:

```sh
cargo run --locked -p workspace-desktop --no-default-features --features gpui --example gpui_chat_probe
```

Должно появиться окно **Workspace GPUI Probe** с текстом «Workspace Desktop
на GPUI». Закрытие окна завершает пример. Пример не читает профиль и не
подключается к Workspace.

Feature `gpui` включает GPUI и общие адаптеры macOS. Тесты с
`#[gpui::test]` могут включить `gpui-test` для доступа к тестовому контексту
GPUI. Точка входа `workspace-desktop-gpui` и примеры `gpui_history_probe`,
`gpui_editor_probe`, `gpui_macos_window_probe`, `gpui_gallery` требуют
`gpui`. Их реализацию добавят следующие карточки.

Прежний клиент Iced остаётся в обычной сборке:

```sh
cargo run --locked -p workspace-desktop --bin workspace-desktop
cargo test --locked --workspace --no-default-features
```
