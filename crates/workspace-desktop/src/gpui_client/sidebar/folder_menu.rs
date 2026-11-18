//! Готовит действия над разговором в контекстном меню каталога.
//! Право на выполнение команды заново проверяется в Core.

use gpui::{App, SharedString};
use workspace_core::messenger::folders::FolderKind;

use super::{Active, FolderCommand, RowKey, Uuid};

/// Намерение из контекстного меню чата.
#[derive(Clone, Eq, PartialEq)]
pub(super) enum RowAction {
    Open,                   // Открыть чат, тему или раскрыть канал.
    Assign,                 // Открыть список пользовательских папок.
    Back,                   // Вернуться к основным действиям.
    Command(FolderCommand), // Изменить связь или закрепление.
}

/// Вариант меню, который можно отрисовать и подтвердить с клавиатуры.
pub(super) struct RowOption {
    pub title: SharedString, // Подпись действия.
    pub action: RowAction,   // Команда или переход внутри меню.
    pub enabled: bool,       // Действие доступно для текущего снимка.
}

/// Список вариантов для текущего уровня контекстного меню.
pub(super) type RowActions = Vec<RowOption>;

impl Active {
    /// Строит действия из актуальных связей, чтобы закрепить именно элемент выбранной папки.
    pub(super) fn row_actions(&self, key: RowKey, assignments: bool, app: &App) -> RowActions {
        let Some((title, enabled)) = self.menu_action(key, app) else {
            return Vec::new();
        };
        let mut actions = vec![option(title, RowAction::Open, enabled)];
        let RowKey::Chat(chat) = key else {
            return actions;
        };
        let Some(folders) = self
            .folder_view
            .as_ref()
            .and_then(|view| view.folders.as_ref())
        else {
            return actions;
        };
        let editable = self.folder_online && self.folder_pending.is_none();
        let scope = if self.folder_contains(chat) {
            self.folder
        } else {
            Uuid::nil()
        };
        if assignments {
            let mut options = vec![option("Назад", RowAction::Back, true)];
            options.extend(
                folders
                    .iter()
                    .filter(|folder| folder.kind == FolderKind::Custom)
                    .map(|folder| {
                        let (title, command) = if let Some(item) =
                            folder.items.iter().find(|item| item.stream_id == chat)
                        {
                            (
                                format!("✓  {}", folder.title),
                                FolderCommand::Remove {
                                    folder: folder.id,
                                    item: item.id,
                                },
                            )
                        } else {
                            (
                                folder.title.clone(),
                                FolderCommand::Add {
                                    folder: folder.id,
                                    chat,
                                },
                            )
                        };
                        option(title, RowAction::Command(command), editable)
                    }),
            );
            return options;
        }
        if let Some(folder) = folders
            .iter()
            .find(|folder| folder.id == scope && folder.kind != FolderKind::Unknown)
            && let Some(item) = folder.items.iter().find(|item| item.stream_id == chat)
        {
            let pinned = item.pinned_at.is_none();
            actions.push(option(
                if pinned {
                    "Закрепить"
                } else {
                    "Открепить"
                },
                RowAction::Command(FolderCommand::Pin {
                    folder: folder.id,
                    item: item.id,
                    pinned,
                }),
                editable,
            ));
        }
        actions.push(option(
            "Добавить в папку  ›",
            RowAction::Assign,
            folders
                .iter()
                .any(|folder| folder.kind == FolderKind::Custom),
        ));
        actions
    }
}

/// Готовит строку меню с итоговой доступностью действия.
fn option(title: impl Into<SharedString>, action: RowAction, enabled: bool) -> RowOption {
    RowOption {
        title: title.into(),
        action,
        enabled,
    }
}
