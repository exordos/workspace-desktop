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
    Assign,                 // Открыть список пользовательских папок.
    Command(FolderCommand), // Изменить связь или закрепление.
    Catalog(CatalogAction), // Изменить разговор или открыть его сведения.
}

/// Вариант меню, который можно отрисовать и подтвердить с клавиатуры.
#[derive(Clone)]
pub(super) struct RowOption {
    pub title: SharedString, // Подпись действия.
    pub action: RowAction,   // Команда или переход внутри меню.
    pub enabled: bool,       // Действие доступно для текущего снимка.
    pub current: bool,       // Выбранный сейчас вариант переключателя.
    pub separator: bool,     // Отделяет действие от предыдущей группы меню.
}

/// Список вариантов для текущего уровня контекстного меню.
pub(super) type RowActions = Vec<RowOption>;

impl Active {
    /// Строит действия из актуальных связей, чтобы закрепить именно элемент выбранной папки.
    pub(super) fn row_actions(&self, key: RowKey, assignments: bool, app: &App) -> RowActions {
        let Some(_) = self.menu_action(key, app) else {
            return Vec::new();
        };
        let mut actions = Vec::new();
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
        actions.extend(actions_for_chat(chat, item, language, editable));
        if let Some(folder) = folders
            .iter()
            .find(|folder| folder.id == scope && folder.kind != FolderKind::Unknown)
            && let Some(item) = folder.items.iter().find(|item| item.stream_id == chat_id)
        {
            let pinned = item.pinned_at.is_none();
            let pin = option(
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
            );
            let notification_count = actions
                .iter()
                .take_while(|entry| {
                    matches!(
                        &entry.action,
                        RowAction::Catalog(CatalogAction::ChatNotifications(_))
                    )
                })
                .count();
            let has_read = actions
                .get(notification_count)
                .is_some_and(|entry| entry.action == RowAction::Catalog(CatalogAction::MarkRead));
            actions.insert(notification_count + usize::from(has_read), pin);
        }
        let folder_action = option(
            format!("{}  ›", language.tr("sidebar.addToFolder")),
            RowAction::Assign,
            folders
                .iter()
                .any(|folder| folder.kind == FolderKind::Custom),
        );
        if let Some(after_members) = actions.iter().position(|entry| {
            matches!(
                &entry.action,
                RowAction::Catalog(
                    CatalogAction::Rename | CatalogAction::NewTopic | CatalogAction::Archive
                )
            )
        }) {
            actions.insert(after_members, folder_action);
        } else {
            actions.push(folder_action);
        }
        if assignments
            && let Some(assign_index) = actions
                .iter()
                .position(|entry| entry.action == RowAction::Assign)
        {
            let folder_options = folders
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
                });
            actions.splice(assign_index + 1..assign_index + 1, folder_options);
        }
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
        current: false,
        separator: false,
    }
}
