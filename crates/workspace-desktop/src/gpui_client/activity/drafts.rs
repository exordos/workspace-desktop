//! Показывает локальные черновики, дополняет их серверными и открывает выбранный UUID.

use std::{collections::HashSet, sync::Arc};

use gpui::{Context, Div, EventEmitter, Render, Task, Window, div, prelude::*};
use uuid::Uuid;
use workspace_core::{
    bootstrap::{ChatList, ChatListStatus},
    messenger::{activity::ActivityDraftEntry, catalog::Chat},
    storage::{DraftScope, DraftVersion, HistoryScope},
    sync::{HistoryRevocation, SyncChange, SyncUpdate},
};

use crate::gpui_client::{
    contracts::ActivityAddress,
    preferences::{Language, Preferences},
    runtime::{LocalDraftError, RuntimeClient},
    ui::{
        Avatar, Button, ButtonAppearance, Icon, PANEL_RADIUS, Palette, TextRole, activate, label,
        popup::tooltip,
    },
};

/// Выбор черновика для редактора указанного разговора.
pub(in crate::gpui_client) struct OpenDraft {
    pub address: ActivityAddress, // Показ, из которого пользователь выбрал карточку.
    pub scope: HistoryScope,      // Адрес проверяемого разговора редактора.
    pub id: Uuid,                 // UUID выбранного черновика для восстановления.
}

/// Одна фоновая операция текущего показа страницы.
struct Work {
    deleting: Option<Uuid>, // None при чтении, UUID при удалении карточки.
    _task: Task<()>,        // Запрос удерживается только пока показана страница.
}

/// Чтение серверного списка после одного локального снимка страницы.
struct ServerLoad {
    id: Uuid,        // Отличает повтор и перечитывание после отзыва доступа в том же показе.
    _task: Task<()>, // Запрос удерживается только пока его результат нужен странице.
}

/// Снимок списка и ожидаемая операция одного показа страницы черновиков.
pub(in crate::gpui_client) struct ActivityDrafts {
    runtime: RuntimeClient, // Рабочий поток с доступом к локальному буферу и хранилищу.
    address: Option<ActivityAddress>, // Показ, к которому относятся данные и запрос.
    catalog: Option<ChatList>, // Подтверждённое название и доступность целей этого входа.
    rows: Vec<ActivityDraftEntry>, // Сохранённые черновики в порядке обновления.
    work: Option<Work>,     // Запущенное чтение или удаление одного черновика.
    server_task: Option<ServerLoad>, // Сервер читается отдельно после показа локальных строк.
    load_failed: bool,      // Последнее чтение завершилось ошибкой.
    server_failed: bool,    // Не удалось обновить серверный список.
    delete_failed: bool,    // Последнее удаление завершилось ошибкой.
    delete_stale: bool,     // Серверная запись изменилась и требует нового выбора.
}

impl EventEmitter<OpenDraft> for ActivityDrafts {}

#[cfg(all(test, feature = "gpui-test"))]
impl ActivityDrafts {
    /// Показывает тесту версию карточки, которая сейчас находится в списке.
    pub(in crate::gpui_client) fn tested_draft_version(&self, id: Uuid) -> Option<u64> {
        self.rows
            .iter()
            .find(|row| row.version.id == id)
            .map(|row| row.version.version)
    }

    /// Подтверждает, что серверный запрос закончился и сообщение о сбое уже занимает место.
    pub(in crate::gpui_client) fn tested_server_failed(&self) -> bool {
        self.server_failed && self.server_task.is_none() && self.work.is_none()
    }
}

impl ActivityDrafts {
    /// Создаёт пустой экран, который ждёт открытия черновиков.
    pub(in crate::gpui_client) fn new(runtime: RuntimeClient) -> Self {
        Self {
            runtime,
            address: None,
            catalog: None,
            rows: Vec::new(),
            work: None,
            server_task: None,
            load_failed: false,
            server_failed: false,
            delete_failed: false,
            delete_stale: false,
        }
    }

    /// Обновляет текущий показ, удаляя строки и незавершённые ответы прежнего входа.
    pub(in crate::gpui_client) fn set_view(
        &mut self,
        address: Option<ActivityAddress>,
        catalog: Option<&ChatList>,
        cx: &mut Context<Self>,
    ) {
        let catalog = catalog.filter(|list| {
            address
                .as_ref()
                .is_some_and(|route| route.session.generation == list.generation)
        });
        let same_catalog = match (self.catalog.as_ref(), catalog) {
            (Some(before), Some(after)) => {
                before.status == after.status
                    && match (&before.chats, &after.chats) {
                        (Some(before), Some(after)) => Arc::ptr_eq(before, after),
                        (None, None) => true,
                        _ => false,
                    }
            }
            (None, None) => true,
            _ => false,
        };
        if !same_catalog {
            self.catalog = catalog.cloned();
            cx.notify();
        }
        if self.address == address {
            return;
        }
        self.address = address;
        self.rows.clear();
        self.work = None;
        self.server_task = None;
        self.load_failed = false;
        self.server_failed = false;
        self.delete_failed = false;
        self.delete_stale = false;
        self.load(cx);
        cx.notify();
    }

    /// Сразу убирает превью, если Core отозвал доступ к разговору или сбросил локальные данные.
    pub(in crate::gpui_client) fn sync_update(
        &mut self,
        update: &SyncUpdate,
        cx: &mut Context<Self>,
    ) {
        if !self.address.as_ref().is_some_and(|route| {
            route.session.owner == update.context && route.session.generation == update.generation
        }) {
            return;
        }
        match update.change {
            SyncChange::HistoryAccessRevoked(HistoryRevocation::Stream(id)) => {
                self.rows.retain(|entry| entry.scope.stream_id != id)
            }
            SyncChange::HistoryAccessRevoked(HistoryRevocation::Topic {
                stream_id,
                topic_id,
            }) => self.rows.retain(|entry| {
                entry.scope.stream_id != stream_id || entry.scope.topic_id != topic_id
            }),
            SyncChange::MessengerReset => self.rows.clear(),
            _ => return,
        }
        self.refresh_after_sync(cx);
    }

    /// После пропуска событий скрывает старый снимок до перечитывания Core.
    pub(in crate::gpui_client) fn sync_lagged(&mut self, cx: &mut Context<Self>) {
        if self.address.is_some() {
            self.rows.clear();
            self.refresh_after_sync(cx);
        }
    }

    /// Перечитывает данные после отзыва, если удаление уже само не запустит чтение.
    fn refresh_after_sync(&mut self, cx: &mut Context<Self>) {
        self.server_task = None;
        if self.work.as_ref().and_then(|work| work.deleting).is_none() {
            self.work = None;
            self.load(cx);
        }
        cx.notify();
    }

    /// Читает локальные черновики, включая последний ввод закрытого редактора.
    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(address) = self.address.clone() else {
            return;
        };
        if self.work.is_some() {
            return;
        }
        let runtime = self.runtime.clone();
        self.server_task = None;
        self.load_failed = false;
        self.server_failed = false;
        let task = cx.spawn(async move |entity, cx| {
            let result = runtime.list_local_drafts(address.session.clone()).await;
            let _ = entity.update(cx, |page, cx| page.finish_load(&address, result, cx));
        });
        self.work = Some(Work {
            deleting: None,
            _task: task,
        });
        cx.notify();
    }

    /// Заменяет список или сохраняет доступный снимок при сбое повторного чтения.
    fn finish_load(
        &mut self,
        address: &ActivityAddress,
        result: Result<Vec<ActivityDraftEntry>, LocalDraftError>,
        cx: &mut Context<Self>,
    ) {
        if self.address.as_ref() != Some(address) {
            return;
        }
        self.work = None;
        match result {
            Ok(rows) => {
                let local: HashSet<_> = rows.iter().map(|row| row.version.id).collect();
                self.rows
                    .retain(|row| row.from_server && !local.contains(&row.version.id));
                self.rows.extend(rows);
                self.rows
                    .sort_by(|first, second| second.updated_at.cmp(&first.updated_at));
                self.load_server(address, cx);
            }
            Err(error) => {
                self.load_failed = true;
                log::debug!(
                    "gpui_shell op_id={} event=draft_list_failed reason={error:?}",
                    address.revision.0
                );
            }
        }
        cx.notify();
    }

    /// Добавляет серверные строки и выдаёт ошибку сверки рядом с локальными данными.
    fn load_server(&mut self, address: &ActivityAddress, cx: &mut Context<Self>) {
        let address = address.clone();
        let runtime = self.runtime.clone();
        let id = Uuid::new_v4();
        let task = cx.spawn(async move |entity, cx| {
            let result = runtime.list_server_drafts(address.session.clone()).await;
            let _ = entity.update(cx, |page, cx| {
                if page.address.as_ref() != Some(&address)
                    || page.server_task.as_ref().map(|pending| pending.id) != Some(id)
                {
                    return;
                }
                page.server_task = None;
                match result {
                    Ok(rows) => {
                        page.rows.retain(|row| !row.from_server);
                        let local: HashSet<_> =
                            page.rows.iter().map(|row| row.version.id).collect();
                        page.rows.extend(
                            rows.into_iter()
                                .filter(|row| !local.contains(&row.version.id)),
                        );
                        page.rows
                            .sort_by(|first, second| second.updated_at.cmp(&first.updated_at));
                    }
                    Err(error) => {
                        page.server_failed = true;
                        log::debug!(
                            "gpui_shell op_id={} event=draft_server_list_failed reason={error:?}",
                            address.revision.0
                        );
                    }
                }
                cx.notify();
            });
        });
        self.server_task = Some(ServerLoad { id, _task: task });
    }

    /// Просит удалить только ту версию черновика, которую пользователь видел на кнопке.
    fn delete(
        &mut self,
        address: &ActivityAddress,
        scope: DraftScope,
        version: DraftVersion,
        from_server: bool,
        cx: &mut Context<Self>,
    ) {
        if self.address.as_ref() != Some(address)
            || self.work.is_some()
            || (from_server && (self.server_task.is_some() || self.server_failed))
            || !self.rows.iter().any(|entry| {
                entry.from_server == from_server
                    && entry.scope == scope
                    && entry.version.id == version.id
                    && entry.version.version == version.version
            })
        {
            return;
        }
        let runtime = self.runtime.clone();
        let origin = address.clone();
        self.delete_failed = false;
        self.delete_stale = false;
        self.server_task = None;
        let task = cx.spawn(async move |entity, cx| {
            let result = if from_server {
                runtime
                    .delete_server_draft(origin.session.clone(), scope, version)
                    .await
            } else {
                runtime
                    .delete_local_draft(origin.session.clone(), scope, version)
                    .await
            };
            let _ = entity.update(cx, |page, cx| {
                if page.address.as_ref() != Some(&origin) {
                    return;
                }
                page.work = None;
                match result {
                    Ok(true) => page.rows.retain(|entry| entry.version.id != version.id),
                    Ok(false) => page.delete_stale = from_server,
                    Err(error) => {
                        page.delete_failed = true;
                        log::debug!(
                            "gpui_shell op_id={} event=draft_delete_failed reason={error:?}",
                            origin.revision.0
                        );
                    }
                }
                page.load(cx);
            });
        });
        self.work = Some(Work {
            deleting: Some(version.id),
            _task: task,
        });
        cx.notify();
    }

    /// Показывает разговор и тему только при наличии цели в каталоге текущего входа.
    fn route(&self, draft: &ActivityDraftEntry) -> Option<(&Chat, Option<&str>, HistoryScope)> {
        let chat = self
            .catalog
            .as_ref()?
            .chats
            .as_ref()?
            .iter()
            .find(|chat| chat.id == draft.scope.stream_id)?;
        let topic = chat
            .topics
            .iter()
            .find(|topic| topic.id == draft.scope.topic_id);
        // Старый кэш мог сохранить личный чат без его тем.
        if topic.is_none() && !(chat.direct && chat.topics.is_empty()) {
            return None;
        }
        Some((
            chat,
            topic.map(|topic| topic.name.as_str()),
            HistoryScope {
                stream_id: chat.id,
                topic_id: topic.map(|topic| topic.id),
            },
        ))
    }

    /// Рисует название чата, время, превью и действия со строкой.
    fn card(
        &self,
        address: &ActivityAddress,
        draft: &ActivityDraftEntry,
        palette: Palette,
        language: Language,
        cx: &Context<Self>,
    ) -> Option<Div> {
        let (chat, topic, scope) = self.route(draft)?;
        let uid = draft.version.id;
        let open_id = format!("activity-draft-open-{uid}");
        let delete_id = format!("activity-draft-delete-{uid}");
        let open = cx.entity().downgrade();
        let delete = open.clone();
        let origin = address.clone();
        let delete_origin = origin.clone();
        let (draft_scope, version) = (draft.scope, draft.version);
        let name = language.chat_title(chat, address.session.user_uuid());
        let title = topic.map_or_else(|| name.to_owned(), |topic| format!("{name} · {topic}"));
        let time = draft
            .updated_at
            .as_ref()
            .and_then(|time| time.as_str().get(11..16));
        let initial = name
            .chars()
            .next()
            .map_or_else(String::new, |letter| letter.to_uppercase().collect());
        let mut trash = Button::new(gpui::SharedString::from(delete_id.clone()), "");
        trash.icon = Some(Icon::Delete);
        trash.appearance = ButtonAppearance::Ghost;
        let from_server = draft.from_server;
        trash.disabled = self.work.is_some()
            || (from_server && (self.server_task.is_some() || self.server_failed));
        let delete_label = if from_server {
            language.tr("desktop.activity.drafts.deleteRemote")
        } else {
            language.tr("desktop.activity.drafts.deleteLocal")
        };
        let storage_label = if from_server {
            language.tr("desktop.activity.drafts.onServer")
        } else {
            language.tr("desktop.activity.drafts.onDevice")
        };
        let card = div()
            .flex()
            .flex_none()
            .items_start()
            .min_w_0()
            .gap_2()
            .p_2()
            .rounded(PANEL_RADIUS)
            .border_1()
            .border_color(palette.border)
            .bg(palette.surface)
            .child(activate(
                div()
                    .id(gpui::SharedString::from(open_id.clone()))
                    .debug_selector(move || open_id.clone())
                    .flex()
                    .items_start()
                    .gap_2()
                    .flex_1()
                    .min_w_0()
                    .p_1()
                    .rounded(PANEL_RADIUS)
                    .border_1()
                    .border_color(gpui::transparent_black())
                    .hover(|style| style.bg(palette.card_hover))
                    .child(Avatar::from_initials(initial).render(palette))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .min_w_0()
                                    .gap_2()
                                    .justify_between()
                                    .child(label(palette, TextRole::Caption, title).truncate())
                                    .children(time.map(|time| {
                                        label(palette, TextRole::Caption, time.to_owned())
                                            .flex_none()
                                    })),
                            )
                            .child(label(
                                palette,
                                TextRole::Body,
                                language.preview_text(&draft.preview),
                            ))
                            .child(label(palette, TextRole::Caption, storage_label)),
                    ),
                None,
                palette,
                move |_, app| {
                    let _ = open.update(app, |view, cx| {
                        if view.address.as_ref() == Some(&origin)
                            && view.work.as_ref().and_then(|work| work.deleting) != Some(uid)
                            && view.rows.iter().any(|entry| {
                                entry.version.id == uid
                                    && entry.version.version == version.version
                                    && entry.from_server == from_server
                                    && view.route(entry).is_some()
                            })
                        {
                            cx.emit(OpenDraft {
                                address: origin.clone(),
                                scope,
                                id: uid,
                            });
                        }
                    });
                },
            ));
        Some(
            card.child(
                trash
                    .render(palette, None, move |_, app| {
                        let _ = delete.update(app, |view, cx| {
                            view.delete(&delete_origin, draft_scope, version, from_server, cx)
                        });
                    })
                    .debug_selector(move || delete_id.clone())
                    .tooltip(move |_, app| tooltip(palette, delete_label.into(), app)),
            ),
        )
    }
}

impl Render for ActivityDrafts {
    /// Прокручивает карточки отдельно от заголовка; сбои оставляют доступные черновики на месте.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pane = div()
            .id("drafts-content")
            .debug_selector(|| "drafts-content".into())
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .gap_2()
            .p_3()
            .overflow_y_scroll();
        let Some(address) = &self.address else {
            return pane;
        };
        let language = Preferences::read_for(cx, &address.session.owner).language;
        let palette = Palette::for_window(window, cx);
        let cards: Vec<_> = self
            .rows
            .iter()
            .filter_map(|draft| self.card(address, draft, palette, language, cx))
            .collect();
        let status = if self.load_failed || self.server_failed {
            let source = cx.entity().downgrade();
            let message = if self.load_failed {
                language.tr("desktop.activity.drafts.loadFailed")
            } else {
                language.tr("desktop.activity.drafts.refreshFailed")
            };
            Some(
                super::status(palette, message, true)
                    .debug_selector(|| "drafts-error".into())
                    .child(
                        Button::new("drafts-retry", language.tr("common.retry")).render(
                            palette,
                            None,
                            move |_, app| {
                                let _ = source.update(app, Self::load);
                            },
                        ),
                    ),
            )
        } else if cards.is_empty() {
            let waiting = self.work.is_some()
                || self.server_task.is_some()
                || self
                    .catalog
                    .as_ref()
                    .is_none_or(|list| list.status == ChatListStatus::Loading);
            let (selector, message) = if waiting {
                ("drafts-loading", language.tr("desktop.activity.drafts.loading"))
            } else {
                ("drafts-empty", language.tr("activity.noDrafts"))
            };
            Some(super::status(palette, message, false).debug_selector(move || selector.into()))
        } else {
            None
        };
        pane.children(status)
            .when(self.delete_stale, |pane| {
                pane.child(
                    super::status(
                        palette,
                        language.tr("desktop.activity.drafts.changed"),
                        false,
                    )
                    .debug_selector(|| "draft-server-delete-changed".into()),
                )
            })
            .when(self.delete_failed, |pane| {
                pane.child(
                    super::status(
                        palette,
                        language.tr("desktop.activity.drafts.deleteFailed"),
                        true,
                    )
                    .debug_selector(|| "draft-delete-error".into()),
                )
            })
            .children(cards)
    }
}
