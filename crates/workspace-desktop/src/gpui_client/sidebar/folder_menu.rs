//! Готовит действия над разговором в контекстном меню каталога.
//! Право на выполнение команды заново проверяется в Core.

use gpui::{App, SharedString};
use workspace_core::messenger::folders::FolderKind;

use super::catalog_actions::{CatalogAction, actions_for_chat, actions_for_topic};
use super::{Active, FolderCommand, RowKey, Uuid};
use crate::gpui_client::preferences::Preferences;

/// Намерение из контекстного меню чата.
#[derive(Clone, Eq, PartialEq)]
pub(super) enum RowAction {
    Open,                   // Открыть чат, тему или раскрыть канал.
    Assign,                 // Открыть список пользовательских папок.
    Back,                   // Вернуться к основным действиям.
    Command(FolderCommand), // Изменить связь или закрепление.
    Catalog(CatalogAction), // Изменить разговор или открыть его сведения.
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
        let Some(chat) = self.chat(key.chat_id()) else {
            return actions;
        };
        let editable = self.folder_online && self.catalog_pending.is_none();
        let language = Preferences::read_for(app, &self.session.owner).language;
        if let RowKey::Topic(_, id) = key {
            if let Some(topic) = chat.topics.iter().find(|topic| topic.id == id) {
                actions.extend(actions_for_topic(chat, topic, language, editable));
            }
            return actions;
        }
        let item = self.folder_item(chat.id);
        let folders = self
            .folder_view
            .as_ref()
            .and_then(|view| view.folders.as_ref());
        let Some(folders) = folders else {
            actions.extend(actions_for_chat(chat, item, language, editable));
            return actions;
        };
        let folder_editable = self.folder_online && self.folder_pending.is_none();
        let chat_id = chat.id;
        let scope = if self.folder_contains(chat_id) {
            self.folder
        } else {
            Uuid::nil()
        };
        if assignments {
            let mut options = vec![option(language.tr("common.back"), RowAction::Back, true)];
            options.extend(
                folders
                    .iter()
                    .filter(|folder| folder.kind == FolderKind::Custom)
                    .map(|folder| {
                        let (title, command) = if let Some(item) =
                            folder.items.iter().find(|item| item.stream_id == chat_id)
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
                                    chat: chat_id,
                                },
                            )
                        };
                        option(title, RowAction::Command(command), folder_editable)
                    }),
            );
            return options;
        }
        if let Some(folder) = folders
            .iter()
            .find(|folder| folder.id == scope && folder.kind != FolderKind::Unknown)
            && let Some(item) = folder.items.iter().find(|item| item.stream_id == chat_id)
        {
            let pinned = item.pinned_at.is_none();
            actions.push(option(
                language.tr(if pinned {
                    "sidebar.pinChat"
                } else {
                    "sidebar.unpinChat"
                }),
                RowAction::Command(FolderCommand::Pin {
                    folder: folder.id,
                    item: item.id,
                    pinned,
                }),
                folder_editable,
            ));
        }
        actions.push(option(
            format!("{}  ›", language.tr("sidebar.addToFolder")),
            RowAction::Assign,
            folders
                .iter()
                .any(|folder| folder.kind == FolderKind::Custom),
        ));
        actions.extend(actions_for_chat(chat, item, language, editable));
        actions
    }
}

/// Готовит строку меню с итоговой доступностью действия.
pub(super) fn option(
    title: impl Into<SharedString>,
    action: RowAction,
    enabled: bool,
) -> RowOption {
    RowOption {
        title: title.into(),
        action,
        enabled,
    }
}
