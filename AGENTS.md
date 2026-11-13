# Workspace Desktop

- `crates/workspace-core` содержит контекст Workspace, авторизацию и тесты жизненного цикла сессии.
- `crates/workspace-desktop` подключает системные адаптеры и содержит UI-компоненты Iced.
- [Архитектура](docs/plans/workspace-desktop-architecture.md) и [контракт авторизации](docs/plans/modules/auth.md) описывают границы модулей.

## Работа с UI

- Для проектирования, реализации или ревью компонентов и экранов используй [Workspace Desktop UI](skills/workspace-desktop-ui/SKILL.md).
- Когда для задачи нужен актуальный макет, используй [Workspace Desktop Figma Bridge](skills/workspace-desktop-figma-bridge/SKILL.md), даже если Figma не упомянута.

## Проверки из корня workspace

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test --workspace --no-default-features
```

[Запуск авторизации без окна](docs/plans/modules/auth.md#запуск-без-окна) описывает проверку на тестовом стенде. Галерея Iced запускается командой `cargo run -p workspace-desktop --example ui_gallery`.
