//! Готовит и рисует ссылочные участки Markdown в одной переносимой строке текста.

use std::{ops::Range, rc::Rc, sync::Arc};

use gpui::{
    Div, ElementId, HighlightStyle, InteractiveText, MouseButton, SharedString, Stateful,
    StyledText, TextLayout, UnderlineStyle, WeakEntity, prelude::*, px,
};
use uuid::Uuid;
use workspace_core::messenger::{
    catalog::Chat,
    details::SidebarDetails,
    references::{
        WorkspaceReferenceTarget as Target, external_link_references,
        workspace_fallback_references, workspace_original_references, workspace_references,
    },
};

use super::{
    Conversation,
    message_menu::MenuHit,
    references::time_zone_hint,
    reply::{ReplyControls, byte_at},
};
use crate::gpui_client::{
    preferences::Language,
    ui::{Palette, TextRole, popup::tooltip, text_role},
};
use crate::platform::message_time::{format_local_time, format_local_time_zone};

/// Часть сообщения со ссылками, разобранная при принятии снимка истории.
#[derive(Eq, PartialEq)]
pub(super) struct InlineText {
    source: SharedString, // Тот же Markdown нужен для промежутков между ссылками.
    references: Vec<Reference>, // Ссылки по возрастанию исходных байтовых позиций.
}

/// Уже прочитанные имена того же входа для старого упоминания, которое содержит только UUID.
#[derive(Clone, Default)]
pub(super) struct MentionNames {
    pub chats: Option<Arc<[Chat]>>, // Собеседники из текущего каталога.
    pub details: Option<Arc<SidebarDetails>>, // Участники из подтверждённых сведений о разговоре.
}

/// Разбирает один уже проверенный диапазон без создания постоянной копии защищённого тела.
pub(super) fn reference_in_range(text: &str, position: Range<usize>) -> Option<Reference> {
    let slice = text.get(position)?;
    references(slice)
        .into_iter()
        .find(|reference| reference.range == (0..slice.len()))
}

impl MentionNames {
    /// Использует карточку участника, а затем имя подтверждённой личной переписки.
    pub(super) fn get(&self, user: Uuid) -> Option<String> {
        let profile = match self.details.as_deref() {
            Some(SidebarDetails::Peer(peer)) if peer.user_id == user => Some(&peer.profile),
            Some(SidebarDetails::Channel(channel)) => channel
                .members
                .iter()
                .find(|member| member.user_id == user)
                .and_then(|member| member.profile.as_ref()),
            _ => None,
        };
        profile
            .map(|profile| profile.display_name.clone())
            .or_else(|| {
                self.chats
                    .as_ref()?
                    .iter()
                    .find(|chat| chat.direct && chat.metadata.direct_peer_id == Some(user))
                    .map(|chat| chat.name.clone())
            })
    }
}

/// Одна ссылка и её публичное отображение до подстановки локального времени.
#[derive(Clone, Eq, PartialEq)]
pub(super) struct Reference {
    pub range: Range<usize>,               // Границы всей конструкции в Markdown.
    pub label_range: Option<Range<usize>>, // Границы подписи при наличии разметки ссылки.
    pub label: String,                     // Подпись после разбора Markdown.
    pub target: Option<Target>,            // None у неподдерживаемой ссылки с видимой подписью.
}

impl Reference {
    /// Переносит проверенные границы в видимый фрагмент того же Markdown.
    pub(super) fn shift(&self, source: Range<usize>, destination: usize) -> Option<Self> {
        if self.range.start < source.start || self.range.end > source.end {
            return None;
        }
        let shift = |range: &Range<usize>| {
            destination + range.start - source.start..destination + range.end - source.start
        };
        Some(Self {
            range: shift(&self.range),
            label_range: self.label_range.as_ref().map(shift),
            ..self.clone()
        })
    }

    /// Подставляет текст упоминания либо локальное время получателя.
    pub(super) fn caption(
        &self,
        language: Language,
        member_name: impl Fn(Uuid) -> Option<String>,
    ) -> String {
        match &self.target {
            Some(Target::User {
                user_id,
                silent: false,
            }) if self.legacy_mention() => {
                let name = member_name(*user_id)
                    .unwrap_or_else(|| language.tr("message.unknownMention").to_owned());
                format!("@{name}")
            }
            Some(Target::User { silent: false, .. }) if !self.label.starts_with('@') => {
                format!("@{}", self.label)
            }
            Some(Target::Time(time)) => format_local_time(time, language.code()),
            _ => self.label.clone(),
        }
    }

    /// Узнаёт положение UUID в старом упоминании, где имя не сохраняется в исходнике.
    pub(super) fn legacy_mention(&self) -> bool {
        self.range.len() == 39
            && self.label_range.as_ref().is_some_and(|range| {
                range.start == self.range.start + 2 && range.end == self.range.end - 1
            })
    }
}

/// Собирает ссылки Core, учитывая, что декодированный веб-адрес также попадает в каталог ссылок.
pub(super) fn references(text: &str) -> Vec<Reference> {
    let workspace = workspace_references(text);
    let mut links = external_link_references(text)
        .into_iter()
        .filter(|link| {
            !workspace
                .iter()
                .any(|reference| reference.range == link.range)
        })
        .map(|link| Reference {
            range: link.range,
            label_range: link.label_range,
            label: link.label,
            target: Some(Target::Url(link.href)),
        })
        .collect::<Vec<_>>();
    links.extend(
        workspace
            .into_iter()
            .filter(|link| !link.image_syntax)
            .map(|link| Reference {
                range: link.range,
                label_range: link.label_range,
                label: link.label,
                target: Some(link.target),
            }),
    );
    links.extend(
        workspace_fallback_references(text)
            .into_iter()
            .map(|link| Reference {
                range: link.range,
                label_range: link.label_range,
                label: link.label,
                target: None,
            }),
    );
    links.sort_by_key(|link| (link.range.start, std::cmp::Reverse(link.range.end)));
    links
}

/// Даёт открытому телу отдельные действия перехода к оригиналам без встроенного содержимого.
pub(super) fn open_references(text: &str) -> Vec<Reference> {
    let mut links = references(text);
    links.extend(
        workspace_original_references(text)
            .into_iter()
            .map(|link| Reference {
                range: link.range,
                label_range: link.label_range,
                label: link.label,
                target: Some(link.target),
            }),
    );
    links.sort_by_key(|link| (link.range.start, std::cmp::Reverse(link.range.end)));
    links
}

/// Сохраняет Markdown буфера обмена, заменяя только признанные элементы времени.
pub(super) fn localize_copied_times(text: String, language: Language) -> String {
    let mut copy = text;
    for reference in workspace_references(&copy).into_iter().rev() {
        if let Target::Time(time) = reference.target {
            copy.replace_range(reference.range, &format_local_time(&time, language.code()));
        }
    }
    copy
}

/// Размещение одной подписи в видимом тексте и её границы в исходном Markdown.
struct VisibleReference {
    original: Range<usize>, // Весь Markdown-элемент в локальных координатах блока.
    display: Range<usize>,  // Заменившие этот элемент байты подписи для GPUI.
    target: Option<Target>, // Действие есть только у разрешённого типа ссылки.
}

/// Параметры показа ссылок в одной строке и допустимые действия.
pub(super) struct InlineRender<'a> {
    pub id: ElementId,      // Ключ одного текстового блока в истории.
    pub row: Uuid,          // Публичный UUID содержащего сообщения.
    pub palette: Palette,   // Цвета текущего бабла.
    pub language: Language, // Язык выбранного аккаунта.
    pub conversation: Option<&'a WeakEntity<Conversation>>, // Владелец действий ссылки.
    pub names: &'a MentionNames, // Уже известные имена старых упоминаний.
    pub selection: Option<InlineSelection<'a>>, // Общая координата выделения открытого текста.
}

/// Привязывает выделение видимых слов к точному исходному телу для цитирования.
pub(super) struct InlineSelection<'a> {
    pub controls: &'a ReplyControls, // Мышь и сохранённый диапазон текущей истории.
    pub body: &'a SharedString,      // Подтверждённое тело, с которым сверит Core.
    pub offset: usize,               // Положение начала этого блока в теле.
}

impl InlineText {
    /// Возвращает заготовку только там, где есть видимая специальная ссылка.
    pub(super) fn new(source: SharedString, offset: usize, whole: &[Reference]) -> Option<Self> {
        let references: Vec<_> = whole
            .iter()
            .filter_map(|link| link.shift(offset..offset + source.len(), 0))
            .collect();
        (!references.is_empty()).then_some(Self { source, references })
    }

    /// Проверяет цель для клика или позднего ответа на открытие сообщения.
    pub(super) fn contains(&self, target: &Target) -> bool {
        self.references
            .iter()
            .any(|link| link.target.as_ref() == Some(target))
    }

    /// Берёт короткий видимый текст для цитаты и сохраняет локаль получателя.
    pub(super) fn visible(&self, language: Language) -> String {
        self.compose(language, |_| None).0
    }

    /// Заменяет ссылочные конструкции подписями и запоминает позиции в видимом тексте.
    fn compose(
        &self,
        language: Language,
        member_name: impl Fn(Uuid) -> Option<String>,
    ) -> (String, Vec<VisibleReference>) {
        let mut visible = String::new();
        let mut targets = Vec::new();
        let mut cursor = 0;
        for link in &self.references {
            if link.range.start < cursor {
                continue;
            }
            visible.push_str(&self.source[cursor..link.range.start]);
            let start = visible.len();
            visible.push_str(&link.caption(language, &member_name));
            targets.push(VisibleReference {
                original: link.range.clone(),
                display: start..visible.len(),
                target: link.target.clone(),
            });
            cursor = link.range.end;
        }
        visible.push_str(&self.source[cursor..]);
        (visible, targets)
    }

    /// Оставляет участки в общей разметке строки и направляет действие в открытый разговор.
    pub(super) fn render(&self, input: InlineRender<'_>) -> (Stateful<Div>, TextLayout) {
        let InlineRender {
            id,
            row,
            palette,
            language,
            conversation,
            names,
            selection,
        } = input;
        let (visible, targets) = self.compose(language, |user| names.get(user));
        let visible = SharedString::from(visible);
        let selection_range = selection.as_ref().and_then(|selection| {
            let range = selection.controls.source_range(
                row,
                selection.body,
                selection.offset..selection.offset + self.source.len(),
            )?;
            Some(display_position(&targets, range.start)..display_position(&targets, range.end))
        });
        let styled = StyledText::new(visible.clone()).with_highlights(highlight_ranges(
            &targets,
            palette,
            selection_range,
        ));
        let layout = styled.layout().clone();
        let targets = Rc::new(targets);
        let hints = targets.clone();
        let mut widget =
            InteractiveText::new((id.clone(), "runs"), styled).tooltip(move |position, _, cx| {
                let Some(Target::Time(time)) = &hints
                    .iter()
                    .find(|link| link.display.contains(&position))?
                    .target
                else {
                    return None;
                };
                let zone = format_local_time_zone(time, language.code());
                let hint = time_zone_hint(language, &zone);
                Some(tooltip(palette, hint.into(), cx))
            });
        let debug_id = id.to_string();
        let mut text = text_role(palette, TextRole::Body)
            .id(id)
            .min_w_0()
            .max_w_full()
            .debug_selector(move || debug_id.clone());
        if let Some(selection) = &selection {
            let controls = selection.controls.clone();
            let body = selection.body.clone();
            let offset = selection.offset;
            let source_end = offset + self.source.len();
            let click_map = targets.clone();
            let click_layout = layout.clone();
            let click_text = visible.clone();
            let drag_map = targets.clone();
            let drag_layout = layout.clone();
            let drag_text = visible.clone();
            let drag = controls.clone();
            text = text
                .cursor(gpui::CursorStyle::IBeam)
                .on_mouse_down(MouseButton::Left, move |event, window, _| {
                    let byte = byte_at(&click_layout, &click_text, event.position);
                    controls.start_selection(
                        row,
                        body.clone(),
                        offset + original_position(&click_map, byte),
                        window,
                    );
                })
                .on_mouse_move(move |event, window, _| {
                    if event.dragging() {
                        let byte = byte_at(&drag_layout, &drag_text, event.position);
                        drag.extend_selection(
                            row,
                            offset..=source_end,
                            offset + original_position(&drag_map, byte),
                            window,
                        );
                    }
                });
        }
        if let Some(conversation) = conversation.cloned() {
            let open = conversation.clone();
            let clicks: Vec<_> = targets
                .iter()
                .filter_map(|link| Some((link.display.clone(), link.target.clone()?)))
                .collect();
            let selection =
                selection.map(|selection| (selection.controls.clone(), selection.body.clone()));
            widget = widget.on_click(
                clicks.iter().map(|(range, _)| range.clone()).collect(),
                move |index, window, cx| {
                    if selection
                        .as_ref()
                        .is_some_and(|(controls, body)| controls.has_selection(row, body))
                    {
                        return;
                    }
                    let _ = open.update(cx, |view, cx| {
                        view.activate_inline_reference(row, &clicks[index].1, window, cx);
                    });
                },
            );
            let mouse_layout = layout.clone();
            text = text.on_mouse_down(MouseButton::Right, move |event, window, cx| {
                let Some(Target::Url(url)) = mouse_layout
                    .index_for_position(event.position)
                    .ok()
                    .and_then(|byte| targets.iter().find(|link| link.display.contains(&byte)))
                    .and_then(|link| link.target.as_ref())
                else {
                    return;
                };
                let _ = conversation.update(cx, |view, cx| {
                    view.open_message_menu(
                        row,
                        event.position,
                        MenuHit::Link(url.clone()),
                        window,
                        cx,
                    );
                });
                cx.stop_propagation();
            });
        }
        (text.child(widget), layout)
    }
}

/// Возвращает исходный байт у курсора. Выделение включает ссылку целиком либо оставляет её снаружи.
fn original_position(targets: &[VisibleReference], shown: usize) -> usize {
    map_position(targets, shown, |link| (&link.display, &link.original))
}

/// Возвращает видимый байт у начала или конца сохранённого выделения.
fn display_position(targets: &[VisibleReference], original: usize) -> usize {
    map_position(targets, original, |link| (&link.original, &link.display))
}

/// Пересчитывает разницу после каждой законченной ссылки, оставляя соседние байты текста точными.
fn map_position(
    targets: &[VisibleReference],
    position: usize,
    ranges: impl Fn(&VisibleReference) -> (&Range<usize>, &Range<usize>),
) -> usize {
    let mut previous = (0, 0);
    for link in targets {
        let (from, to) = ranges(link);
        if position <= from.start {
            break;
        }
        if position < from.end {
            return if position - from.start < from.end - position {
                to.start
            } else {
                to.end
            };
        }
        previous = (from.end, to.end);
    }
    previous.1 + (position - previous.0)
}

/// Разводит пересекающиеся выделение и оформление ссылки по неперекрывающимся частям.
fn highlight_ranges(
    targets: &[VisibleReference],
    palette: Palette,
    selection: Option<Range<usize>>,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let active = targets
        .iter()
        .filter(|link| link.target.is_some() && !link.display.is_empty());
    let mut breaks: Vec<_> = active
        .clone()
        .flat_map(|link| [link.display.start, link.display.end])
        .collect();
    if let Some(range) = &selection {
        breaks.extend([range.start, range.end]);
    }
    breaks.sort_unstable();
    breaks.dedup();
    let mut links = active.peekable();
    let mut styles = Vec::new();
    for pair in breaks.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        while links.peek().is_some_and(|link| link.display.end <= start) {
            links.next();
        }
        let link = links
            .peek()
            .filter(|link| link.display.contains(&start))
            .and_then(|link| link.target.as_ref());
        let selected = selection
            .as_ref()
            .is_some_and(|range| range.contains(&start));
        if selected || link.is_some() {
            let mut style = link
                .map(|target| link_style(target, palette))
                .unwrap_or_default();
            style.background_color = selected.then_some(palette.accent_soft);
            styles.push((start..end, style));
        }
    }
    styles
}

/// Помечает ссылку цветом и подчёркиванием в рамках одной роли текста.
fn link_style(target: &Target, palette: Palette) -> HighlightStyle {
    HighlightStyle {
        color: Some(palette.accent),
        underline: (!matches!(target, Target::User { .. } | Target::Time(_))).then_some(
            UnderlineStyle {
                thickness: px(1.),
                ..UnderlineStyle::default()
            },
        ),
        ..HighlightStyle::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Показывает обычные и тихие упоминания, веб-ссылку и безопасную подпись неизвестной схемы.
    #[test]
    fn references_compose_text_without_exposing_destinations() {
        let id = Uuid::from_u128(6);
        let source = format!(
            "До [Аня](urn:user:{id}) и [Борис](urn:user:{id} \"silent\") <@{id}> [сайт](urn:url:https://example.com) [битая](urn:topic:x) после"
        );
        let links = open_references(&source);
        let inline = InlineText::new(SharedString::from(source.clone()), 0, &links).unwrap();
        let (shown, targets) = inline.compose(Language::Ru, |id| {
            (id == Uuid::from_u128(6)).then(|| "Из каталога".into())
        });
        assert_eq!(shown, "До @Аня и Борис @Из каталога сайт битая после");
        assert_eq!(
            targets
                .iter()
                .filter(|target| target.target.is_some())
                .count(),
            4
        );
        assert_eq!(
            original_position(&targets, shown.find("после").unwrap()),
            source.find("после").unwrap()
        );
        let mention = &targets[0];
        assert_eq!(
            &source[original_position(&targets, mention.display.start)
                ..original_position(&targets, mention.display.end)],
            format!("[Аня](urn:user:{id})")
        );
    }

    /// Оставляет код и экранированное время в исходном Markdown, переводя только видимый момент.
    #[test]
    fn localizes_copied_times_only_in_rendered_markdown() {
        let source = "`urn:time:2026-10-13T10:02:00Z` и urn:time:2026-10-13T10:02:00Z";
        let copied = localize_copied_times(source.to_owned(), Language::Ru);
        assert!(copied.starts_with("`urn:time:2026-10-13T10:02:00Z` и "));
        assert_eq!(copied.matches("urn:time:").count(), 1);
    }
}
