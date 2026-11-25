//! Разбирает защищённый Markdown в отзываемые диапазоны текста, цитаты и карточки файлов.

use std::{ops::Range, sync::Arc};

use gpui::{AnyElement, App, Context, Div, IntoElement, WeakEntity, Window, div, prelude::*, px};
use uuid::Uuid;
use workspace_core::{
    messenger::{
        encryption::ProtectedMessage,
        quote::{self, Block},
        references::{WorkspaceReferenceTarget, media_references},
    },
    storage::media::MediaKind,
};

use super::{
    Conversation,
    attachments::{AttachmentKey, Cards, ProtectedAttachment},
    inline_references::{
        MentionNames, Reference, TIME_CHIP_ICON, TIME_CHIP_RADIUS, reference_in_range, references,
    },
    media_album,
    reply::history_quote,
    viewport::GuardedText,
};
use crate::gpui_client::{
    preferences::Language,
    ui::{Icon, Palette, TextRole, activate, text_role},
};

/// Структура Markdown для одного отзываемого представления. Содержит только позиции и UUID.
pub(super) struct ProtectedLayout(Vec<Part>); // Отображаемые блоки в порядке исходного текста.

/// Один блок защищённого Markdown.
enum Part {
    Text(Range<usize>), // Диапазон тела, которое перечитывается непосредственно при layout.
    RichText(Vec<InlinePart>), // Диапазоны видимого текста и подписей ссылок без их содержимого.
    Quote(Uuid),        // Публичный UUID исходного сообщения, которое проверит Core.
    File {
        id: Uuid,        // Публичный ID вложения.
        kind: MediaKind, // Общий тип карточки файла.
        position: usize, // Место ссылки внутри исходного сообщения.
        ratio: f32,      // Пропорции кадра без имени и URL защищённого файла.
    },
}

/// Сегмент текста под тем же разрешением ключа.
enum InlinePart {
    Text(Range<usize>),                     // Неизменившийся исходный текст.
    Reference(Range<usize>, ReferenceKind), // Разрешённый вид ссылки без подписи и адреса.
}

/// Вид оформления защищённой ссылки, определённый при разборе.
#[derive(Clone, Copy)]
enum ReferenceKind {
    Caption, // Подпись ссылки без действия.
    Link,    // Обычная ссылка с переходом.
    Time,    // Плашка с локальной датой и часами.
}

/// Отзываемый источник нажатия. Содержит позицию, но не открытый URL или отображаемую подпись.
#[derive(Clone)]
pub(super) struct ProtectedInline {
    pub row: Uuid,                     // Сообщение, где пользователь нажал ссылку.
    pub access: Arc<ProtectedMessage>, // Действующая версия исходного конверта.
    pub range: Range<usize>,           // Границы элемента в неизменном теле этой версии.
}

impl ProtectedInline {
    /// Читает проверенную цель только на момент явного действия пользователя.
    pub(super) fn target(&self) -> Option<WorkspaceReferenceTarget> {
        self.access
            .with_body(|text| reference_in_range(text, self.range.clone())?.target)
            .flatten()
    }
}

/// Действия, локаль и уже известные имена текущего окна.
pub(super) struct ProtectedReferenceControls<'a> {
    pub conversation: Option<&'a WeakEntity<Conversation>>, // Владелец проверок открытия цели.
    pub language: Language,                                 // Язык подписей и локального времени.
    pub names: &'a MentionNames, // Опубликованный справочник этого показа.
}

impl ProtectedLayout {
    /// Берёт те же структурные Markdown блоки, которые используют обычные сообщения.
    pub(super) fn parse(text: &str) -> Self {
        let media = media_references(text);
        let links = references(text);
        let mut parts = Vec::new();
        for block in quote::blocks(text) {
            match block {
                Block::Quote(quote) => parts.push(Part::Quote(quote.message_id)),
                Block::Text(block) => {
                    let start = block.as_ptr().addr() - text.as_ptr().addr();
                    let end = start + block.len();
                    let mut cursor = start;
                    for (position, file) in media
                        .iter()
                        .enumerate()
                        .filter(|(_, file)| file.range.start >= start && file.range.end <= end)
                    {
                        append_text(text, cursor..file.range.start, &links, &mut parts);
                        parts.push(Part::File {
                            id: file.file_id,
                            kind: file.kind,
                            position,
                            ratio: media_album::reference_ratio(file),
                        });
                        cursor = file.range.end;
                    }
                    append_text(text, cursor..end, &links, &mut parts);
                }
            }
        }
        Self(parts)
    }

    /// Проверяет повторный клик по той же ссылке, сохранившейся в текущем теле.
    pub(super) fn has_file(&self, key: AttachmentKey) -> bool {
        self.0.iter().any(|part| matches!(
            part, Part::File { id, position, .. } if *id == key.file && *position == key.position
        ))
    }

    /// Возвращает указатели цитат, не удерживая подписи и фрагменты из оригинального текста.
    pub(super) fn quotes(&self) -> impl Iterator<Item = Uuid> + '_ {
        self.0.iter().filter_map(|part| match part {
            Part::Quote(id) => Some(*id),
            _ => None,
        })
    }

    /// Даёт альбому позиции прямых медиа и их пропорции без открытой ссылки.
    pub(super) fn album(&self) -> impl Iterator<Item = (usize, f32)> + '_ {
        self.0.iter().filter_map(|part| match part {
            Part::File {
                kind: MediaKind::Image | MediaKind::Video,
                position,
                ratio,
                ..
            } => Some((*position, *ratio)),
            _ => None,
        })
    }

    /// Находит исходный файл по позиции плитки в альбоме.
    pub(super) fn album_file(&self, index: usize) -> Option<(Uuid, MediaKind)> {
        self.0.iter().find_map(|part| match part {
            Part::File {
                id, kind, position, ..
            } if *position == index => Some((*id, *kind)),
            _ => None,
        })
    }

    /// Узнаёт, останутся ли текст, цитаты или документы после выноса медиа наверх.
    pub(super) fn has_footer(&self) -> bool {
        self.0.iter().any(|part| {
            !matches!(
                part,
                Part::File {
                    kind: MediaKind::Image | MediaKind::Video,
                    ..
                }
            )
        })
    }

    /// Собирает те же роли текста, цитат и карточек, оставляя исходные строки в Core.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render(
        &self,
        id: Uuid,
        access: &Arc<ProtectedMessage>,
        palette: Palette,
        own: bool,
        media: Option<(&Cards, &mut App, Language)>,
        separate_album: bool,
        references: ProtectedReferenceControls<'_>,
        quote: impl Fn(usize, Palette, bool, Option<(&Cards, &mut App, Language)>) -> AnyElement,
    ) -> Div {
        let mut media = media;
        let mut content = div().flex().flex_col().min_w_0().max_w_full().gap_1();
        let mut quote_position = 0;
        for part in &self.0 {
            let item = match part {
                Part::Text(range) => text_role(palette, TextRole::Body)
                    .min_w_0()
                    .max_w_full()
                    .child(GuardedText::range(access.clone(), range.clone()))
                    .into_any_element(),
                Part::RichText(parts) => {
                    render_reference_line(id, access, palette, parts, &references)
                        .into_any_element()
                }
                Part::Quote(_) => {
                    let media = media
                        .as_mut()
                        .map(|(cards, app, language)| (*cards, &mut **app, *language));
                    let item = quote(quote_position, palette, own, media);
                    quote_position += 1;
                    item
                }
                Part::File {
                    id: file,
                    kind,
                    position,
                    ..
                } => {
                    if separate_album && matches!(kind, MediaKind::Image | MediaKind::Video) {
                        continue;
                    }
                    let Some((cards, app, language)) = media.as_mut() else {
                        continue;
                    };
                    let key = AttachmentKey {
                        message: id,
                        file: *file,
                        position: *position,
                    };
                    cards
                        .render_protected(
                            ProtectedAttachment::direct(key, access.clone()),
                            *kind,
                            None,
                            palette,
                            *language,
                            app,
                        )
                        .into_any_element()
                }
            };
            content = content.child(item);
        }
        content
    }
}

/// Пропускает пустую подпись рядом с карточкой. Отступы остаются теми же, что у обычного Markdown.
fn append_text(text: &str, range: Range<usize>, links: &[Reference], parts: &mut Vec<Part>) {
    if !text
        .get(range.clone())
        .is_some_and(|text| !text.trim().is_empty())
    {
        return;
    }
    let mut inline = Vec::new();
    let mut cursor = range.start;
    for link in links
        .iter()
        .filter(|link| link.range.start >= range.start && link.range.end <= range.end)
    {
        if link.range.start < cursor {
            continue;
        }
        if cursor < link.range.start {
            inline.push(InlinePart::Text(cursor..link.range.start));
        }
        inline.push(InlinePart::Reference(
            link.range.clone(),
            match link.target {
                Some(WorkspaceReferenceTarget::Time(_)) => ReferenceKind::Time,
                Some(_) => ReferenceKind::Link,
                None => ReferenceKind::Caption,
            },
        ));
        cursor = link.range.end;
    }
    if inline.is_empty() {
        parts.push(Part::Text(range));
    } else {
        if cursor < range.end {
            inline.push(InlinePart::Text(cursor..range.end));
        }
        parts.push(Part::RichText(inline));
    }
}

/// Привязывает нажатия к отзыву ключа и рисует подписи через защищённые фазы GPUI.
fn render_reference_line(
    row: Uuid,
    access: &Arc<ProtectedMessage>,
    palette: Palette,
    parts: &[InlinePart],
    controls: &ProtectedReferenceControls<'_>,
) -> Div {
    div()
        .flex()
        .flex_wrap()
        .min_w_0()
        .max_w_full()
        .children(parts.iter().map(|part| {
            match part {
                InlinePart::Text(range) => text_role(palette, TextRole::Body)
                    .min_w_0()
                    .max_w_full()
                    .child(GuardedText::range(access.clone(), range.clone()))
                    .into_any_element(),
                InlinePart::Reference(range, kind) => {
                    let is_time = matches!(kind, ReferenceKind::Time);
                    let item = text_role(palette, TextRole::Body)
                        .min_w_0()
                        .max_w_full()
                        .when(is_time, |item| {
                            item.flex()
                                .items_center()
                                .gap(px(4.))
                                .px(px(5.))
                                .rounded(TIME_CHIP_RADIUS)
                                .border_1()
                                .border_color(palette.border)
                                .bg(palette.surface)
                                .child(Icon::MessageTime.element(TIME_CHIP_ICON, palette.text))
                        })
                        .child(GuardedText::reference(
                            access.clone(),
                            range.clone(),
                            controls.language,
                            controls.names.clone(),
                        ));
                    let Some(conversation) = controls
                        .conversation
                        .cloned()
                        .filter(|_| !matches!(kind, ReferenceKind::Caption))
                    else {
                        return item.into_any_element();
                    };
                    let source = ProtectedInline {
                        row,
                        access: access.clone(),
                        range: range.clone(),
                    };
                    let language = controls.language;
                    let item = item
                        .id(("history-protected-inline", range.start))
                        .when(!is_time, |item| item.text_color(palette.accent))
                        .when(is_time, |item| {
                            let source = source.clone();
                            item.hover(move |style| style.bg(palette.card_hover))
                                .tooltip(move |_, cx| {
                                    cx.new(|_| ProtectedTimeHint {
                                        source: source.clone(),
                                        language,
                                        palette,
                                    })
                                    .into()
                                })
                        });
                    activate(item, None, palette, move |window, cx| {
                        cx.stop_propagation();
                        let _ = conversation.update(cx, |view, cx| {
                            view.activate_protected_reference(&source, window, cx)
                        });
                    })
                    .into_any_element()
                }
            }
        }))
}

/// Подсказка хранит разрешение и диапазон вместо открытой подписи часового пояса.
struct ProtectedTimeHint {
    source: ProtectedInline, // Сообщение и неизменный диапазон даты.
    language: Language,      // Язык часового пояса.
    palette: Palette,        // Тема истории при открытии подсказки.
}

impl Render for ProtectedTimeHint {
    /// Показывает пояс, только пока источник разрешает чтение.
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        text_role(self.palette, TextRole::Caption)
            .px_2()
            .py_1()
            .rounded(TIME_CHIP_RADIUS)
            .border_1()
            .border_color(self.palette.border)
            .bg(self.palette.surface)
            .child(GuardedText::reference_zone(
                self.source.access.clone(),
                self.source.range.clone(),
                self.language,
            ))
    }
}

/// Показывает ожидающую цитату через ту же рамку, что и обычный оригинал.
pub(super) fn unavailable_quote(palette: Palette, own: bool, language: Language) -> AnyElement {
    history_quote(
        palette,
        own,
        language.tr("composer.quote").into(),
        "".into(),
        false,
        language,
    )
    .into_any_element()
}
