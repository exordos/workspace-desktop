//! Показывает защищённые цитаты только от проверенного оригинала, включая ссылку на другую тему.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use gpui::{AnyElement, ClickEvent, Context, IntoElement, WeakEntity, div, prelude::*, px};
use uuid::Uuid;
use workspace_core::{
    messenger::{encryption::ProtectedMessage, quote::ProtectedQuote},
    storage::{HistoryMessage, HistoryScope, media::MediaKind},
};

use super::{
    Conversation, State,
    attachments::{AttachmentKey, Cards, ProtectedAttachment},
    protected_body::unavailable_quote,
    viewport::GuardedText,
};
use crate::gpui_client::{
    preferences::Language,
    ui::{
        Palette, QuoteSurface, TextRole, activate_click, click_is_stationary, ellipsized_label,
        quote_frame_with_header, text_role,
    },
};

/// Исходная цитата под порядковым номером внутри конкретного сообщения.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(super) struct QuoteSlot {
    pub message: Uuid,   // UUID защищённого бабла.
    pub position: usize, // Порядковый номер цитаты среди ссылок в её теле.
}

/// Убирает поздний вывод Core, когда этот результат больше не принадлежит открытому баблу.
#[derive(Clone)]
pub(in crate::gpui_client) struct QuoteText {
    content: Arc<ProtectedQuote>, // Отзывный материал оригинала, закрытый от остальных поверхностей.
    mounted: Arc<AtomicBool>,     // Удаление, правка и уход из окна снимают представление.
    source: Uuid,                 // UUID оригинала нужен для точного файлового запроса.
}

impl QuoteText {
    /// Проверяет доступ к обоим телам до конца одного действия GPUI.
    pub(super) fn with_current<T>(&self, action: impl FnOnce() -> T) -> Option<T> {
        self.mounted
            .load(Ordering::Acquire)
            .then(|| self.content.with_current(action))
            .flatten()
    }

    /// Отдаёт видимую часть только безопасному элементу, повторяя проверку исходного доступа.
    pub(super) fn with_text<T>(
        &self,
        placeholder: &str,
        read: impl FnOnce(&str) -> T,
    ) -> Option<T> {
        self.mounted
            .load(Ordering::Acquire)
            .then(|| self.content.with_text(placeholder, read))
            .flatten()
    }

    /// Передаёт Core проверенные версии цитаты и оригинала для загрузки вложения.
    pub(super) fn media_quote(&self) -> Option<&ProtectedQuote> {
        self.mounted
            .load(Ordering::Acquire)
            .then_some(self.content.as_ref())
    }

    /// Привязывает отложенный кадр к той же попытке разрешения цитаты.
    pub(super) fn same_location(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.mounted, &other.mounted)
    }

    /// Возвращает UUID оригинала, только пока действуют ключ и размещение цитаты.
    pub(super) fn current_source(&self) -> Option<Uuid> {
        self.with_current(|| self.source)
    }

    /// Показывает только допустимые UUID и типы файлов из целого оригинала.
    fn attachments(&self) -> Vec<(Uuid, MediaKind)> {
        if !self.mounted.load(Ordering::Acquire) {
            return Vec::new();
        }
        self.content
            .with_media_references(|files| files.to_vec())
            .unwrap_or_default()
    }
}

/// Действующая метаинформация оригинала и возможный отзываемый вид для UI.
struct Resolved {
    scope: HistoryScope,  // Проверенное размещение исходного сообщения.
    key_id: Option<Uuid>, // Поколение для перечитывания даже у закрытого оригинала.
    own: bool,            // Автор оригинала совпадает с текущим входом.
    content: Option<Arc<ProtectedQuote>>, // Результат с независимым временем жизни GPUI.
}

/// Состояние этой ссылки в текущем окне истории.
struct Entry {
    source: Uuid, // Точный оригинал, который должна прочитать рабочая задача.
    parent: Arc<ProtectedMessage>, // Идентичность версии цитирующего конверта.
    citing: Arc<HistoryMessage>, // Каноническая версия цитирующей строки из текущего окна.
    operation: Uuid, // UUID попытки, который отсеивает запоздалый результат.
    mounted: Arc<AtomicBool>, // Защищает отложенный замер и paint старого ответа.
    resolved: Option<Resolved>, // Только принятые сведения от Core.
}

impl Drop for Entry {
    /// Останавливает вывод, когда исчезла актуальная связь с исходным баблом.
    fn drop(&mut self) {
        self.mounted.store(false, Ordering::Release);
    }
}

/// Цитаты из текущих раскрытых сообщений, отдельно от канонического снимка окна.
#[derive(Default)]
pub(super) struct ProtectedQuotes(HashMap<QuoteSlot, Entry>); // Точное размещение и версия каждой цитаты.

/// Представление оригинала для отложенных фаз GPUI.
#[derive(Clone)]
pub(super) struct QuotePresentation {
    access: QuoteText, // Совместная проверка оригинала и цитирующего ключа.
    own: bool,         // Публичная подпись автора принадлежит текущему входу.
}

impl ProtectedQuotes {
    /// Снимает все результаты после ухода с маршрута или сброса подписки.
    pub(super) fn clear(&mut self) {
        self.0.clear();
    }

    /// Отзывает исходник, который изменил Sync, и все цитаты из изменившегося конверта.
    pub(super) fn invalidate_message(&mut self, message: Uuid) -> Vec<Uuid> {
        self.invalidate(|slot, entry| slot.message == message || entry.source == message)
    }

    /// Перечитывает затронутые попытки и отдельно отмечает ранее показанные строки.
    fn invalidate_expired_origin(&mut self, message: Uuid) -> (bool, Vec<Uuid>) {
        let mut removed = false;
        let rows = self.invalidate(|slot, entry| {
            let stale = (slot.message == message || entry.source == message)
                && entry
                    .resolved
                    .as_ref()
                    .and_then(|resolved| resolved.content.as_ref())
                    .is_none_or(|content| content.with_current(|| ()).is_none());
            removed |= stale;
            stale
        });
        (removed, rows)
    }

    /// Отзывает только сведения оригиналов затронутого стрима и поколения.
    pub(super) fn invalidate_key(&mut self, stream: Uuid, key: Uuid) -> Vec<Uuid> {
        self.invalidate(|_, entry| {
            entry.resolved.as_ref().is_none_or(|resolved| {
                resolved.scope.stream_id == stream && resolved.key_id == Some(key)
            })
        })
    }

    /// Убирает результат, когда изменился доступ к показанной теме оригинала.
    pub(super) fn invalidate_scope(&mut self, stream: Uuid, topic: Option<Uuid>) -> Vec<Uuid> {
        self.invalidate(|_, entry| {
            entry.resolved.as_ref().is_none_or(|resolved| {
                resolved.scope.stream_id == stream
                    && topic.is_none_or(|topic| resolved.scope.topic_id == Some(topic))
            })
        })
    }

    /// Возвращает именно баблы, которым надо сбросить измерение после снятия сведений.
    fn invalidate(&mut self, mut remove: impl FnMut(QuoteSlot, &Entry) -> bool) -> Vec<Uuid> {
        let mut changed = Vec::new();
        self.0.retain(|slot, entry| {
            let stale = remove(*slot, entry);
            if stale
                && entry
                    .resolved
                    .as_ref()
                    .is_some_and(|resolved| resolved.content.is_some())
            {
                changed.push(slot.message);
            }
            !stale
        });
        changed
    }

    /// Выдаёт только доступные оригиналы, сохраняя разрешения вместе с представлением.
    pub(super) fn presentations(&self) -> HashMap<QuoteSlot, QuotePresentation> {
        self.0
            .iter()
            .filter_map(|(slot, entry)| {
                let source = entry.resolved.as_ref()?;
                Some((
                    *slot,
                    QuotePresentation {
                        access: QuoteText {
                            content: source.content.as_ref()?.clone(),
                            mounted: entry.mounted.clone(),
                            source: entry.source,
                        },
                        own: source.own,
                    },
                ))
            })
            .collect()
    }

    /// Проверяет, что токен всё ещё принадлежит той же позиции и тому же оригиналу.
    pub(super) fn has_current_source(
        &self,
        slot: QuoteSlot,
        expected: &QuoteText,
        target: Uuid,
    ) -> bool {
        target != slot.message
            && self.0.get(&slot).is_some_and(|entry| {
                entry.source == target
                    && Arc::ptr_eq(&entry.mounted, &expected.mounted)
                    && entry
                        .resolved
                        .as_ref()
                        .and_then(|resolved| resolved.content.as_ref())
                        .is_some_and(|content| Arc::ptr_eq(content, &expected.content))
                    && expected.current_source() == Some(target)
            })
    }
}

/// Использует общую рамку цитаты с отдельным отзываемым текстом.
pub(super) fn render_quote(
    projection: Option<&QuotePresentation>,
    palette: Palette,
    own: bool,
    slot: QuoteSlot,
    language: Language,
    media: Option<(&Cards, &mut gpui::App, Language)>,
    conversation: Option<&WeakEntity<Conversation>>,
) -> AnyElement {
    let Some(projection) =
        projection.filter(|projection| projection.access.with_current(|| ()).is_some())
    else {
        return unavailable_quote(palette, own, language);
    };
    let surface = if own {
        QuoteSurface::TintedMessage
    } else {
        QuoteSurface::Message
    };
    let author = language.tr(if projection.own {
        "common.you"
    } else {
        "desktop.messenger.participant"
    });
    let access = projection.access.clone();
    let text = GuardedText::quote(access.clone(), language.tr("composer.quote"));
    let title_palette = Palette {
        text: palette.focus,
        ..palette
    };
    let header = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            ellipsized_label(title_palette, TextRole::Label, author)
                .debug_selector(|| "quote-frame-heading".into()),
        )
        .child(text_role(palette, TextRole::Body).child(text));
    let header = if let Some(conversation) = conversation.cloned() {
        let access = access.clone();
        activate_click(
            div()
                .id(gpui::ElementId::named_usize(
                    format!("history-protected-quote-open-{}", slot.message),
                    slot.position,
                ))
                .border_1()
                .border_color(gpui::transparent_black())
                .rounded(px(4.))
                .child(header),
            None,
            palette,
            move |event: &ClickEvent, _, cx| {
                cx.stop_propagation();
                if click_is_stationary(event) {
                    let _ = conversation.update(cx, |view, cx| {
                        view.open_protected_quote(slot, access.clone(), cx)
                    });
                }
            },
        )
        .debug_selector(move || {
            format!("history-protected-quote-{}-{}", slot.message, slot.position)
        })
        .into_any_element()
    } else {
        header
            .debug_selector(move || {
                format!("history-protected-quote-{}-{}", slot.message, slot.position)
            })
            .into_any_element()
    };
    let mut body = div().flex().flex_col().gap_1();
    if let Some((cards, app, language)) = media {
        for (position, (file, kind)) in projection.access.attachments().into_iter().enumerate() {
            let key = AttachmentKey {
                message: projection.access.source,
                file,
                position,
            };
            let attachment = ProtectedAttachment::quoted(key, slot, projection.access.clone());
            body =
                body.child(cards.render_protected(attachment, kind, None, palette, language, app));
        }
    }
    quote_frame_with_header(palette, surface, header, body)
        .debug_selector(move || {
            format!(
                "history-protected-quote-frame-{}-{}",
                slot.message, slot.position
            )
        })
        .into_any_element()
}

impl Conversation {
    /// Проверяет доступ к тому же фрагменту, который может измерить кадр.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(in crate::gpui_client) fn tested_protected_quote_text(
        &self,
        message: Uuid,
    ) -> Option<String> {
        self.encryption
            .as_ref()?
            .quotes
            .presentations()
            .get(&QuoteSlot {
                message,
                position: 0,
            })?
            .access
            .with_text("Цитата", str::to_owned)
    }

    /// Снимает устаревшие оригиналы после изменения ключа в любой теме текущего входа.
    pub(super) fn quoted_key_changed(&mut self, stream: Uuid, key: Uuid, cx: &mut Context<Self>) {
        for row in self.viewport.fragment_views.invalidate_key(stream, key, cx) {
            self.viewport.invalidate_row_height(row);
        }
        if let Some(encryption) = &mut self.encryption {
            let rows = encryption.quotes.invalidate_key(stream, key);
            self.refresh_quote_rows(rows, cx);
            self.resolve_protected_quotes(cx);
        }
    }

    /// Запрашивает оригиналы вне потока GPUI и проверяет, что цитата ещё относится к открытому окну.
    pub(super) fn resolve_protected_quotes(&mut self, cx: &mut Context<Self>) {
        let (State::Ready(history), Some(encryption), Some(runtime)) =
            (&self.state, &mut self.encryption, &self.avatar_runtime)
        else {
            return;
        };
        let requests = encryption.quote_requests();
        let visible: HashMap<_, _> = history
            .messages
            .iter()
            .map(|message| (message.id, message))
            .collect();
        encryption.quotes.0.retain(|slot, entry| {
            requests.get(slot).is_some_and(|(source, parent)| {
                *source == entry.source && Arc::ptr_eq(parent, &entry.parent)
            }) && visible
                .get(&slot.message)
                .is_some_and(|citing| citing.same_content(entry.citing.as_ref()))
        });
        let mut captured = HashMap::new();
        for (slot, (source, parent)) in requests {
            if encryption.quotes.0.contains_key(&slot) {
                continue;
            }
            let Some(citing) = visible.get(&slot.message).copied() else {
                continue;
            };
            let citing = captured
                .entry(slot.message)
                .or_insert_with(|| Arc::new(citing.clone()))
                .clone();
            let operation = Uuid::new_v4();
            encryption.quotes.0.insert(
                slot,
                Entry {
                    source,
                    parent: parent.clone(),
                    citing: citing.clone(),
                    operation,
                    mounted: Arc::new(AtomicBool::new(true)),
                    resolved: None,
                },
            );
            let address = self.address.clone();
            let runtime = runtime.clone();
            let activation = encryption.activation.clone();
            cx.spawn(async move |entity, cx| {
                let reply = runtime
                    .conversation(address, move |services, _| async move {
                        services
                            .messenger
                            .resolve_protected_quote(
                                &services.sync,
                                &activation,
                                parent,
                                &citing,
                                source,
                                slot.position,
                            )
                            .await
                            .ok()
                    })
                    .await;
                let _ = entity.update(cx, |view, cx| {
                    let Some(resolved) = reply
                        .for_current(Some(&view.address))
                        .and_then(Result::ok)
                        .flatten()
                    else {
                        return;
                    };
                    let Some(encryption) = &mut view.encryption else {
                        return;
                    };
                    let Some(entry) = encryption.quotes.0.get_mut(&slot).filter(|entry| {
                        entry.operation == operation && entry.parent.with_current(|| ()).is_some()
                    }) else {
                        return;
                    };
                    let content = resolved
                        .content
                        .filter(|content| content.with_current(|| ()).is_some())
                        .map(Arc::new);
                    let ready = content.is_some();
                    entry.resolved = Some(Resolved {
                        scope: resolved.scope,
                        key_id: resolved.key_id,
                        own: resolved.own,
                        content,
                    });
                    if ready {
                        view.viewport.invalidate_row_height(slot.message);
                        cx.notify();
                    }
                });
            })
            .detach();
        }
    }

    /// Обновляет только строки с отозванной версией цитаты или оригинала.
    pub(super) fn quoted_origin_changed(&mut self, message: Uuid, cx: &mut Context<Self>) {
        let Some(encryption) = &mut self.encryption else {
            return;
        };
        let (removed, rows) = encryption.quotes.invalidate_expired_origin(message);
        if removed {
            self.refresh_quote_rows(rows, cx);
            self.resolve_protected_quotes(cx);
        }
    }

    /// Сбрасывает размеры только разомкнувшихся зависимых цитат.
    pub(super) fn refresh_quote_rows(&mut self, rows: Vec<Uuid>, cx: &mut Context<Self>) {
        for &id in &rows {
            self.viewport.invalidate_row_height(id);
        }
        if !rows.is_empty() {
            self.attachments.invalidate_revoked(cx);
            cx.notify();
        }
    }
}
