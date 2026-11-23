//! Расшифровывает превью отдельно от каталога и обновляет только нужные строки.

use std::{cell::Cell, collections::HashMap, rc::Rc, sync::Arc};

use gpui::{Context, Entity, IntoElement, Render, Task, Window, div, prelude::*};
use tokio::sync::broadcast::error::RecvError;
use uuid::Uuid;
use workspace_core::messenger::{
    catalog::{Chat, ProtectedSidebarSummary, SidebarPreview, SidebarPreviewBody},
    encryption::{DecryptionError, EncryptionActivation, MessageEncryption},
};

use super::Sidebar;
use crate::gpui_client::{
    conversation::GuardedText,
    preferences::Preferences,
    ui::{Palette, TextRole, label, text_role},
};

type PreviewId = (Uuid, Uuid, Uuid);

/// Готовность подписи, видимой только одной строкой текущего каталога.
enum State {
    Locked,                              // Устройство ещё не получило ключ.
    Failed,                              // Точный конверт не открывается.
    Ready(Arc<ProtectedSidebarSummary>), // Сам текст доступен через отзывной держатель Core.
}

/// Отдельное представление второй строки одного элемента каталога.
pub(super) struct SecurePreview {
    state: State, // Глифы зависят от ключа независимо от рендера родительской строки.
    sender: Option<String>, // Подпись автора из публичного снимка.
    mounted: Rc<Cell<bool>>, // Отзывы каталога прекращают вывод уже собранного элемента.
}

impl Render for SecurePreview {
    /// Формирует подпись в текущей теме оформления и проверяет ключ ещё раз при выводе.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let locale = Preferences::read(cx).language;
        let palette = Palette::for_window(window, cx);
        let text = match &self.state {
            State::Ready(summary) if summary.with_text(|_| ()).is_some() && self.mounted.get() => {
                text_role(palette, TextRole::Caption)
                    .text_color(palette.text_secondary)
                    .min_w_0()
                    .flex_1()
                    .overflow_hidden()
                    .child(GuardedText::summary(summary.clone(), self.mounted.clone()))
                    .debug_selector(|| "catalog-encrypted-preview-ready".into())
            }
            State::Failed => label(
                palette,
                TextRole::Caption,
                locale.text("Не удалось расшифровать", "Could not decrypt"),
            ),
            _ => label(
                palette,
                TextRole::Caption,
                locale.text("Нужен ключ", "Key required"),
            )
            .debug_selector(|| "catalog-encrypted-preview-locked".into()),
        };
        div()
            .flex()
            .min_w_0()
            .items_center()
            .overflow_hidden()
            .children(self.sender.as_ref().map(|sender| {
                label(palette, TextRole::Caption, format!("{sender}: ")).whitespace_nowrap()
            }))
            .child(text)
    }
}

/// Сохранённый конверт и результат отдельной попытки для UUID каталога.
struct Entry {
    preview: SidebarPreview, // Последний известный снимок, включая точный конверт.
    attempt: Uuid,           // При смене ключа прежняя подготовка уже не применяется.
    mounted: Rc<Cell<bool>>, // Логический срок жизни тела одного превью.
    view: Entity<SecurePreview>, // Дочерний Render, который уведомляем при смене состояния.
}

impl Drop for Entry {
    /// Закрывает отложенную отрисовку старого тела при замене каталога.
    fn drop(&mut self) {
        self.mounted.set(false);
    }
}

/// Подписка и только актуальные превью выбранного входа.
pub(super) struct ProtectedPreviews {
    activation: EncryptionActivation, // Точная активация сессии, которую понимает Core.
    engine: Arc<MessageEncryption>,   // Общий источник ключей профиля.
    entries: HashMap<PreviewId, Entry>, // Стрим, тема и сообщение.
    _watch: Task<()>,                 // Завершается при смене входа.
}

#[cfg(all(test, feature = "gpui-test"))]
impl Sidebar {
    /// Проверяет настоящий отзывный текст одного превью в приёмке каталога.
    pub(in crate::gpui_client) fn tested_protected_summary(
        &self,
        id: Uuid,
        app: &gpui::App,
    ) -> Option<String> {
        let previews = self.active.as_ref()?.protected_previews.as_ref()?;
        let entry = previews
            .entries
            .values()
            .find(|entry| entry.preview.message_id == id)?;
        let State::Ready(summary) = &entry.view.read(app).state else {
            return None;
        };
        summary.with_text(str::to_owned)
    }
}

impl ProtectedPreviews {
    /// Захватывает сессию до чтения ключей и отбрасывает одноимённые события старого выбора.
    pub(super) fn new(
        engine: Arc<MessageEncryption>,
        activation: EncryptionActivation,
        cx: &mut Context<Sidebar>,
    ) -> Self {
        let mut updates = engine.subscribe();
        let observed = activation.clone();
        let watcher = cx.spawn(async move |sidebar, cx| {
            loop {
                let (scope, available) = match updates.recv().await {
                    Ok(change) if change.activation == observed => {
                        (Some((change.stream_id, change.key_id)), change.available)
                    }
                    Ok(_) => continue,
                    Err(RecvError::Lagged(_)) => (None, true),
                    Err(RecvError::Closed) => break,
                };
                if sidebar
                    .update(cx, |sidebar, cx| {
                        let Some(previews) = sidebar
                            .active
                            .as_mut()
                            .and_then(|active| active.protected_previews.as_mut())
                            .filter(|previews| previews.activation == observed)
                        else {
                            return;
                        };
                        previews.key_changed(scope, available, cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            activation,
            engine,
            entries: HashMap::new(),
            _watch: watcher,
        }
    }

    /// Проверяет, что после повторного выбора не переиспользуется прежний держатель.
    pub(super) fn is_activation(&self, activation: &EncryptionActivation) -> bool {
        &self.activation == activation
    }

    /// Отдаёт представление для того же размещения, на которое ведёт строка.
    pub(super) fn view(
        &self,
        stream: Uuid,
        preview: Option<&SidebarPreview>,
    ) -> Option<Entity<SecurePreview>> {
        let preview = preview?;
        self.entries
            .get(&(stream, preview.topic_id, preview.message_id))
            .map(|entry| entry.view.clone())
    }

    /// Сопоставляет новые подписи по точному телу и повторно использует незатронутые представления.
    pub(super) fn update(&mut self, chats: &[Chat], cx: &mut Context<Sidebar>) {
        let mut current = HashMap::new();
        for chat in chats {
            let previews = chat.metadata.preview.iter().chain(
                chat.topics
                    .iter()
                    .filter_map(|topic| topic.metadata.preview.as_ref()),
            );
            for preview in previews {
                if matches!(preview.body(), SidebarPreviewBody::Encrypted { .. }) {
                    current.insert((chat.id, preview.topic_id, preview.message_id), preview);
                }
            }
        }
        self.entries.retain(|id, entry| {
            current
                .get(id)
                .is_some_and(|preview| **preview == entry.preview)
        });
        let mut changed = Vec::new();
        for (id, preview) in current {
            if self.entries.contains_key(&id) {
                continue;
            }
            let sender = if preview.author_id.to_string() == self.activation.session().owner.user_id
            {
                Some("Вы".into())
            } else {
                preview.author_name.clone()
            };
            let mounted = Rc::new(Cell::new(true));
            let view = cx.new(|_| SecurePreview {
                state: State::Locked,
                sender,
                mounted: mounted.clone(),
            });
            self.entries.insert(
                id,
                Entry {
                    preview: preview.clone(),
                    attempt: Uuid::nil(),
                    mounted,
                    view,
                },
            );
            changed.push(id);
        }
        self.prepare(changed, cx);
    }

    /// Меняет только подписи, использующие упомянутый стрим и точное поколение ключа.
    fn key_changed(
        &mut self,
        scope: Option<(Uuid, Uuid)>,
        available: bool,
        cx: &mut Context<Sidebar>,
    ) {
        let mut changed = Vec::new();
        for (&id, entry) in &mut self.entries {
            let SidebarPreviewBody::Encrypted { key_id, .. } = entry.preview.body() else {
                continue;
            };
            if scope.is_some_and(|scope| scope != (id.0, *key_id)) {
                continue;
            }
            entry.attempt = Uuid::nil();
            entry.view.update(cx, |view, cx| {
                view.state = State::Locked;
                cx.notify();
            });
            if available {
                changed.push(id);
            }
        }
        self.prepare(changed, cx);
    }

    /// Системное чтение ключа и разбор подписи выполняются до обновления Entity.
    fn prepare(&mut self, ids: Vec<PreviewId>, cx: &mut Context<Sidebar>) {
        let requests: Vec<_> = ids
            .into_iter()
            .filter_map(|id| {
                let entry = self.entries.get_mut(&id)?;
                let operation = Uuid::new_v4();
                entry.attempt = operation;
                Some((id, operation, entry.preview.clone()))
            })
            .collect();
        if requests.is_empty() {
            return;
        }
        let engine = self.engine.clone();
        let activation = self.activation.clone();
        let captured = activation.clone();
        let preparation = cx.background_executor().spawn(async move {
            requests
                .into_iter()
                .map(|(id, operation, preview)| {
                    (
                        id,
                        operation,
                        preview.prepare_summary(id.0, &engine, &captured),
                    )
                })
                .collect::<Vec<_>>()
        });
        cx.spawn(async move |sidebar, cx| {
            let results = preparation.await;
            let _ = sidebar.update(cx, |sidebar, cx| {
                let Some(previews) = sidebar
                    .active
                    .as_mut()
                    .and_then(|active| active.protected_previews.as_mut())
                    .filter(|previews| previews.activation == activation)
                else {
                    return;
                };
                for (id, operation, result) in results {
                    let Some(entry) = previews
                        .entries
                        .get(&id)
                        .filter(|entry| entry.attempt == operation)
                    else {
                        continue;
                    };
                    let state = match result {
                        Ok(Some(summary)) => State::Ready(Arc::new(summary)),
                        Err(DecryptionError::InvalidCiphertext | DecryptionError::KeyStore(_)) => {
                            State::Failed
                        }
                        _ => State::Locked,
                    };
                    entry.view.update(cx, |view, cx| {
                        view.state = state;
                        cx.notify();
                    });
                }
            });
        })
        .detach();
    }
}
