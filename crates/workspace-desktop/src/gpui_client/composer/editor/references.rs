//! Показывает подсказки для @ и # и хранит UUID выбранных ссылок рядом с читаемым текстом поля.

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    sync::Arc,
};

use gpui::{
    Context, Corner, Deferred, ElementId, Pixels, Point, Stateful, Window, anchored, deferred, div,
    point, prelude::*, px,
};
use uuid::Uuid;
use workspace_core::{
    context::OwnerContext,
    directory::UserProfile,
    messenger::{
        catalog::Chat,
        encryption::fragments::protected_draft_spans,
        references::{
            WorkspaceReferenceTarget, build_workspace_reference_markdown, media_references,
            parse_workspace_conversation_link, workspace_reference_prose_at, workspace_references,
        },
    },
};
use zeroize::Zeroizing;

use super::Editor;
use crate::{
    gpui_client::{
        preferences::{Language, Preferences},
        ui::{Palette, TextRole, label, popup},
    },
    platform::message_time::format_local_time,
};

const MAX_SUGGESTIONS: usize = 6;

/// Каноническая ссылка и диапазон читаемого текста в одном состоянии редактора.
pub(super) struct ReferenceSpan {
    range: Range<usize>,         // Байты видимой подписи в поле.
    markdown: Zeroizing<String>, // Точное исходное написание ссылки для отправки и черновика.
    file_id: Option<Uuid>,       // UUID для управления вложением из ряда под редактором.
}

/// Подпись свежего файла, которую нужно сохранить при смене вкладки и разметки.
struct TemporaryFile {
    markdown: Zeroizing<String>, // Точная ссылка, выданная Core для этого файла.
    label: String,               // Видимая подпись и представление ссылки в буфере обмена.
}

/// Текущая команда поиска в точке ввода.
struct Query<'a> {
    range: Range<usize>, // Знак и поисковая строка, которые заменятся при выборе.
    people: bool,        // Истина означает @; иначе поиск по чатам и темам.
    text: &'a str,       // Часть после знака.
}

/// Один доступный человеку результат из каталога, с точной целью для отправки.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Suggestion {
    target: WorkspaceReferenceTarget, // Цель и вид канонической ссылки.
    name: String,                     // Подпись ссылки и основная строка в списке.
    detail: Option<String>,           // Почта помогает различить одинаковые имена.
    outside: bool, // Полный список участников подтверждает отсутствие автора ссылки.
}

/// Привязка редактора к тем же справочникам, которыми пользуется открытый аккаунт.
#[derive(Default)]
pub(super) struct References {
    owner: Option<OwnerContext>, // Владелец каталога и разрешённый адрес для вставки URL.
    chats: Option<Arc<[Chat]>>,  // Последний снимок доступных чатов.
    people: Vec<(Uuid, UserProfile)>, // Видимые пользователи из общего справочника.
    members: Option<HashSet<Uuid>>, // Подтверждённый состав канала, если сервер его вернул.
    suggestions_enabled: bool,   // Есть подтверждённая открытая тема для вставки @ и #.
    active: usize,               // Подсвеченный пункт текущего поиска.
    dismissed_at: Option<(u64, usize)>, // Версия и курсор поиска, закрытого Escape.
    spans: Vec<ReferenceSpan>,   // Размещённые ссылки в порядке следования.
    canonical: Option<Zeroizing<String>>, // Текст на проводе, когда в поле есть читаемые подписи.
    temporary_files: HashMap<Uuid, TemporaryFile>, // Ссылки, которые нельзя раскрыть из редактора.
}

impl References {
    /// Разбирает только известные ссылки при восстановлении, сохраняя остальной Markdown побайтно.
    pub(super) fn display_markup(
        &self,
        markdown: &str,
        language: Language,
    ) -> (String, Vec<ReferenceSpan>) {
        let mut visible = String::new();
        let mut spans = Vec::new();
        let mut start = 0;
        let mut entries: Vec<_> = workspace_references(markdown)
            .into_iter()
            .filter(|reference| !reference.image_syntax)
            .filter_map(|reference| {
                let display = match &reference.target {
                    WorkspaceReferenceTarget::User { silent: false, .. } => {
                        format!("@{}", reference.label)
                    }
                    WorkspaceReferenceTarget::User { silent: true, .. }
                    | WorkspaceReferenceTarget::Stream(_)
                    | WorkspaceReferenceTarget::Topic { .. } => reference.label,
                    WorkspaceReferenceTarget::Time(time) => {
                        format_local_time(time, language.code())
                    }
                    _ => return None,
                };
                Some((reference.range, display, None))
            })
            .collect();
        entries.extend(media_references(markdown).into_iter().map(|reference| {
            let display = reference.name.unwrap_or(reference.label);
            (reference.range, display, Some(reference.file_id))
        }));
        // Код или другая разметка могут скрыть ссылку от парсера. Подпись временного файла в
        // редакторе и буфере остаётся подписью даже после восстановления такого текста.
        let temporary: Vec<_> = self
            .temporary_files
            .iter()
            .flat_map(|(id, file)| {
                markdown
                    .match_indices(file.markdown.as_str())
                    .map(move |(offset, text)| {
                        (offset..offset + text.len(), file.label.clone(), Some(*id))
                    })
            })
            .collect();
        if !temporary.is_empty() {
            entries.retain(|(existing, _, _)| {
                !temporary
                    .iter()
                    .any(|(range, _, _)| range.start < existing.end && range.end > existing.start)
            });
            entries.extend(temporary);
        }
        entries.sort_unstable_by_key(|(range, _, _)| range.start);
        for (range, display, file_id) in entries {
            if range.start < start {
                continue;
            }
            if display.is_empty() || display.contains(['\n', '\r']) {
                continue;
            }
            visible.push_str(&markdown[start..range.start]);
            let span_start = visible.len();
            visible.push_str(&display);
            spans.push(ReferenceSpan {
                range: span_start..visible.len(),
                markdown: Zeroizing::new(markdown[range.clone()].to_owned()),
                file_id,
            });
            start = range.end;
        }
        visible.push_str(&markdown[start..]);
        (visible, spans)
    }

    /// Вставляет новые подписи и сдвигает остальные. Редактирование подписи превращает её в текст.
    pub(super) fn replaced(
        &mut self,
        range: Range<usize>,
        inserted: usize,
        visible: &str,
        mut spans: Vec<ReferenceSpan>,
    ) {
        let removed = range.end - range.start;
        self.spans.retain_mut(|span| {
            if span.range.end <= range.start {
                true
            } else if span.range.start >= range.end {
                span.range =
                    (span.range.start - removed + inserted)..(span.range.end - removed + inserted);
                true
            } else {
                false
            }
        });
        for span in &mut spans {
            span.range = span.range.start + range.start..span.range.end + range.start;
        }
        self.spans.extend(spans);
        self.spans.sort_unstable_by_key(|span| span.range.start);
        self.canonical = (!self.spans.is_empty())
            .then(|| Zeroizing::new(self.markdown_range(visible, 0..visible.len())));
        self.active = 0;
        self.dismissed_at = None;
    }

    /// Возвращает исходный Markdown для полных ссылок и обычный текст для части подписи.
    pub(super) fn markdown_range(&self, visible: &str, range: Range<usize>) -> String {
        self.serialize_range(visible, range, false)
    }

    /// Переносит позицию в Markdown на видимый текст, где ссылка занимает место подписи.
    pub(super) fn display_offset(spans: &[ReferenceSpan], offset: usize) -> usize {
        let mut wire = 0;
        let mut visible = 0;
        for span in spans {
            let start = wire + span.range.start - visible;
            if offset <= start {
                break;
            }
            let end = start + span.markdown.len();
            if offset < end {
                return span.range.start;
            }
            wire = end;
            visible = span.range.end;
        }
        visible + offset - wire
    }

    /// Копирует готовые ссылки и подписи файлов, которые ещё принадлежат редактору.
    pub(super) fn clipboard_range(&self, visible: &str, range: Range<usize>) -> String {
        self.serialize_range(visible, range, true)
    }

    /// Собирает ссылки в выделении, пропуская временные UUID при записи в буфер обмена.
    fn serialize_range(&self, visible: &str, range: Range<usize>, copying: bool) -> String {
        let mut result = String::new();
        let mut start = range.start;
        for span in &self.spans {
            if span.range.start >= range.start
                && span.range.end <= range.end
                && !(copying
                    && span
                        .file_id
                        .is_some_and(|id| self.temporary_files.contains_key(&id)))
            {
                result.push_str(&visible[start..span.range.start]);
                result.push_str(&span.markdown);
                start = span.range.end;
            }
        }
        result.push_str(&visible[start..range.end]);
        result
    }

    /// Даёт отправке и черновикам исходный Markdown или исходный текст при вводе без ссылок.
    pub(super) fn markdown<'a>(&'a self, visible: &'a str) -> &'a str {
        self.canonical.as_deref().map_or(visible, String::as_str)
    }

    /// Проверяет, исчезнет ли ссылка после замены участка с совпадающим видимым текстом.
    pub(super) fn touches_span(&self, range: &Range<usize>) -> bool {
        self.spans
            .iter()
            .any(|span| span.range.start < range.end && span.range.end > range.start)
    }

    /// Убирает копии исходного текста при отзыве разрешения на правку.
    pub(super) fn clear_content(&mut self) {
        self.spans.clear();
        self.temporary_files.clear();
        self.canonical = None;
        self.active = 0;
        self.dismissed_at = None;
    }

    /// Ранжирует людей по подтверждённому участию, совпадению имени и стабильному UUID.
    fn mentioned_people(&self, text: &str) -> Vec<Suggestion> {
        let text = text.to_lowercase();
        let mut entries: Vec<_> = self
            .people
            .iter()
            .filter_map(|(id, profile)| {
                let lower = profile.display_name.to_lowercase();
                let name_match = lower.contains(&text);
                let email_match = profile
                    .email
                    .as_ref()
                    .is_some_and(|email| email.to_lowercase().contains(&text));
                if !name_match && !email_match {
                    return None;
                }
                let outside = self
                    .members
                    .as_ref()
                    .is_some_and(|members| !members.contains(id));
                Some((outside, !lower.starts_with(&text), lower, *id, profile))
            })
            .collect();
        entries.sort_unstable_by(|a, b| (&a.0, &a.1, &a.2, a.3).cmp(&(&b.0, &b.1, &b.2, b.3)));
        entries
            .into_iter()
            .take(MAX_SUGGESTIONS)
            .map(|(outside, _, _, id, profile)| Suggestion {
                target: WorkspaceReferenceTarget::User {
                    user_id: id,
                    silent: false,
                },
                name: profile.display_name.clone(),
                detail: profile.email.clone(),
                outside,
            })
            .collect()
    }

    /// Возвращает совпавшие чаты и темы из снимка текущего аккаунта.
    fn conversations(&self, text: &str) -> Vec<Suggestion> {
        let text = text.to_lowercase();
        let Some(chats) = &self.chats else {
            return Vec::new();
        };
        let streams = chats
            .iter()
            .filter(|chat| chat.name.to_lowercase().contains(&text))
            .map(|chat| Suggestion {
                target: WorkspaceReferenceTarget::Stream(chat.id),
                name: chat.name.clone(),
                detail: None,
                outside: false,
            });
        let topics = chats.iter().flat_map(|chat| {
            chat.topics.iter().filter_map(|topic| {
                let name = format!("{} › {}", chat.name, topic.name);
                format!("{} {}", chat.name, topic.name)
                    .to_lowercase()
                    .contains(&text)
                    .then_some(Suggestion {
                        target: WorkspaceReferenceTarget::Topic {
                            topic_id: topic.id,
                            stream_id: None,
                        },
                        name,
                        detail: None,
                        outside: false,
                    })
            })
        });
        streams.chain(topics).take(MAX_SUGGESTIONS).collect()
    }

    /// Превращает отдельные URL доступных чатов в канонические ссылки, оставляя остальной текст.
    fn convert_paste(&self, text: &str) -> String {
        let (Some(owner), Some(chats)) = (&self.owner, &self.chats) else {
            return text.to_owned();
        };
        let mut result = String::new();
        let mut copied = 0;
        let mut offset = 0;
        for part in text.split_inclusive(char::is_whitespace) {
            let remainder = part.trim_end_matches(char::is_whitespace);
            let url = remainder.trim_end_matches(['.', ',', '!', '?', ';', ':']);
            if let Some((stream_id, topic_id)) = parse_workspace_conversation_link(url, owner)
                && let Some(chat) = chats.iter().find(|chat| chat.id == stream_id)
                && let Some((label, target)) = pasted_target(chat, topic_id)
                && let Some(markdown) = build_workspace_reference_markdown(&label, &target)
            {
                result.push_str(&text[copied..offset]);
                result.push_str(&markdown);
                copied = offset + url.len();
            }
            offset += part.len();
        }
        result.push_str(&text[copied..]);
        result
    }
}

/// Получает подпись из доступного чата и проверяет принадлежность темы именно этому чату.
fn pasted_target(
    chat: &Chat,
    topic_id: Option<Uuid>,
) -> Option<(String, WorkspaceReferenceTarget)> {
    if let Some(id) = topic_id {
        let topic = chat.topics.iter().find(|topic| topic.id == id)?;
        Some((
            format!("{} › {}", chat.name, topic.name),
            WorkspaceReferenceTarget::Topic {
                topic_id: id,
                stream_id: None,
            },
        ))
    } else {
        Some((chat.name.clone(), WorkspaceReferenceTarget::Stream(chat.id)))
    }
}

/// Ищет @ или # у курсора на границе слова, сохраняя позиции UTF-8.
fn query_at(text: &str, caret: usize) -> Option<Query<'_>> {
    let before = text.get(..caret)?;
    let start = before.rfind(['@', '#'])?;
    let trigger = before.as_bytes()[start];
    let prefix = &before[..start];
    let text = &before[start + 1..];
    if text
        .chars()
        .any(|character| character.is_whitespace() || matches!(character, '@' | '#' | '`'))
        || prefix
            .chars()
            .next_back()
            .is_some_and(|character| !character.is_whitespace() && !"([{,.:;!?".contains(character))
        || prefix
            .rsplit('\n')
            .next()
            .is_some_and(|line| line.matches('`').count() % 2 != 0)
    {
        return None;
    }
    Some(Query {
        range: start..caret,
        people: trigger == b'@',
        text,
    })
}

impl Editor {
    /// Настраивает редактор рабочего разговора при первом подключении.
    pub(in crate::gpui_client::composer) fn set_reference_owner(&mut self, owner: OwnerContext) {
        self.references.owner = Some(owner);
    }

    /// Меняет доступность подсказок, сохраняя читаемые ссылки в шифрованном тексте.
    pub(in crate::gpui_client::composer) fn set_reference_suggestions(
        &mut self,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        if self.references.suggestions_enabled != enabled {
            self.references.suggestions_enabled = enabled;
            self.references.active = 0;
            cx.notify();
        }
    }

    /// Передаёт доступные чаты из каталога того же входа и обновляет открытые подсказки.
    pub(in crate::gpui_client::composer) fn set_reference_chats(
        &mut self,
        chats: Option<Arc<[Chat]>>,
        cx: &mut Context<Self>,
    ) {
        if self.references.chats == chats {
            return;
        }
        self.references.chats = chats;
        cx.notify();
    }

    /// Получает профили и признак подтверждённого членства только из ответа для текущего входа.
    pub(in crate::gpui_client::composer) fn set_reference_people(
        &mut self,
        people: Vec<(Uuid, UserProfile)>,
        members: Option<HashSet<Uuid>>,
        cx: &mut Context<Self>,
    ) {
        self.references.people = people;
        self.references.members = members;
        cx.notify();
    }

    /// Сбрасывает сведения о членстве при изменении канала или все результаты при отзыве доступа.
    pub(in crate::gpui_client::composer) fn invalidate_reference_people(
        &mut self,
        revoked: bool,
        cx: &mut Context<Self>,
    ) {
        self.references.members = None;
        if revoked {
            self.references.people.clear();
            self.references.chats = None;
        }
        cx.notify();
    }

    /// Проверяет, нужен ли текущему запросу @ справочник людей.
    pub(in crate::gpui_client::composer) fn is_mention_query(&self) -> bool {
        self.reference_query().is_some_and(|query| query.people)
    }

    /// Возвращает язык текущего аккаунта для даты и подписей.
    pub(super) fn reference_language(&self, cx: &Context<Self>) -> Language {
        self.references.owner.as_ref().map_or_else(
            || Preferences::read(cx).language,
            |owner| Preferences::read_for(cx, owner).language,
        )
    }

    /// Вставляет канонический Markdown и показывает читаемые подписи известных ссылок.
    pub(super) fn replace_markup(
        &mut self,
        range: Range<usize>,
        markdown: &str,
        cx: &mut Context<Self>,
    ) {
        if self.references.owner.is_none() {
            self.replace(range, markdown, cx);
            return;
        }
        let (visible, spans) = self
            .references
            .display_markup(markdown, self.reference_language(cx));
        self.replace_with_spans(range, &visible, spans, cx);
    }

    /// Вставляет новую ссылку на время в точку ввода или выбранный диапазон.
    pub(in crate::gpui_client::composer) fn insert_time(
        &mut self,
        time: workspace_core::storage::HistoryTime,
        cx: &mut Context<Self>,
    ) {
        if self.can_format()
            && let Some(markdown) =
                build_workspace_reference_markdown("", &WorkspaceReferenceTarget::Time(time))
        {
            let range = self.selection();
            let before = self.text[..range.start].chars().next_back();
            let after = self.text[range.end..].chars().next();
            let left = before
                .filter(|character| !character.is_whitespace())
                .map_or("", |_| " ");
            let right = if after.is_none_or(|character| !character.is_whitespace()) {
                " "
            } else {
                ""
            };
            self.replace_markup(range, &format!("{left}{markdown}{right}"), cx);
        }
    }

    /// Вставляет изображение в позицию drop, если Markdown прочитает его как вложение.
    pub(in crate::gpui_client::composer) fn insert_inline_attachment(
        &mut self,
        markdown: &str,
        point: Point<Pixels>,
        temporary: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(layout) = self.current_layout() else {
            return false;
        };
        if self.composing() {
            return false;
        }
        let index = layout.index_at(point);
        if self.references.touches_span(&(index..index)) {
            return false;
        }
        // Позиция окна относится к подписи. Проверять нужно полный исходный Markdown.
        let offset = self.references.markdown_range(&self.text, 0..index).len();
        let mut candidate = Zeroizing::new(self.text().to_owned());
        candidate.insert_str(offset, markdown);
        let Some(reference) = media_references(&candidate)
            .into_iter()
            .find(|reference| reference.range == (offset..offset + markdown.len()))
        else {
            return false;
        };
        if temporary {
            self.references.temporary_files.insert(
                reference.file_id,
                TemporaryFile {
                    markdown: Zeroizing::new(markdown.to_owned()),
                    label: reference.name.unwrap_or(reference.label),
                },
            );
        }
        self.replace_markup(index..index, markdown, cx);
        true
    }

    /// Ставит файл отдельным абзацем перед текстом при проверке готового Markdown.
    #[cfg(all(test, feature = "gpui-test"))]
    pub(in crate::gpui_client::composer) fn insert_attachment(
        &mut self,
        markdown: &str,
        cx: &mut Context<Self>,
    ) {
        let insertion = self.leading_attachment_end();
        let before = if insertion == 0 { "" } else { "\n\n" };
        let after = if insertion == 0 && !self.text.is_empty() {
            "\n\n"
        } else {
            ""
        };
        let old_len = self.text.len();
        let anchor = self.anchor;
        let caret = self.caret;
        let marked = self.marked.clone();
        self.replace_markup(
            insertion..insertion,
            &format!("{before}{markdown}{after}"),
            cx,
        );
        let added = self.text.len() - old_len;
        let shift = |offset| {
            if offset < insertion {
                offset
            } else {
                offset + added
            }
        };
        self.anchor = shift(anchor);
        self.caret = shift(caret);
        self.marked = marked.map(|range| shift(range.start)..shift(range.end));
    }

    /// Находит конец начального ряда файлов, сохраняя порядок нескольких загрузок.
    #[cfg(all(test, feature = "gpui-test"))]
    fn leading_attachment_end(&self) -> usize {
        let mut end = 0;
        for span in &self.references.spans {
            let separator = &self.text[end..span.range.start];
            let isolated = span.range.end == self.text.len()
                || self.text[span.range.end..].starts_with("\n\n");
            let expected = if end == 0 { "" } else { "\n\n" };
            if span.file_id.is_none() || separator != expected || !isolated {
                break;
            }
            end = span.range.end;
        }
        end
    }

    /// Читает только видимые файлы этого черновика, отличая повторы одного UUID по позиции.
    pub(in crate::gpui_client::composer) fn attachments(
        &self,
    ) -> impl Iterator<Item = (Uuid, usize, &str)> {
        self.references.spans.iter().filter_map(|span| {
            Some((
                span.file_id?,
                span.range.start,
                &self.text[span.range.clone()],
            ))
        })
    }

    /// Убирает точное вложение и оставляет соседние подписи и ссылки на месте.
    pub(in crate::gpui_client::composer) fn remove_attachment(
        &mut self,
        id: Uuid,
        start: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(mut range) = self
            .references
            .spans
            .iter()
            .find(|span| span.file_id == Some(id) && span.range.start == start)
            .map(|span| span.range.clone())
        else {
            return;
        };
        if self.text[..range.start].ends_with("\n\n")
            && (range.end == self.text.len() || self.text[range.end..].starts_with("\n\n"))
        {
            range.start -= 2;
        } else if range.start == 0 && self.text[range.end..].starts_with("\n\n") {
            range.end += 2;
        }
        self.erase_attachment_range(range, cx);
    }

    /// Убирает все вставки временных файлов, сохраняя курсор и незатронутую IME-композицию.
    pub(in crate::gpui_client::composer) fn remove_attachment_files(
        &mut self,
        files: &[Uuid],
        cx: &mut Context<Self>,
    ) {
        self.references
            .temporary_files
            .retain(|id, _| !files.contains(id));
        let ranges: Vec<_> = self
            .references
            .spans
            .iter()
            .rev()
            .filter(|span| span.file_id.is_some_and(|id| files.contains(&id)))
            .map(|span| span.range.clone())
            .collect();
        for range in ranges {
            self.erase_attachment_range(range, cx);
        }
    }

    /// Убирает конкретную подпись старого вложения, оставляя другие ссылки на этот файл.
    pub(in crate::gpui_client::composer) fn remove_attachment_link(
        &mut self,
        markdown: &str,
        cx: &mut Context<Self>,
    ) {
        let protected = if self.fragment_edit.is_some() {
            protected_draft_spans(self.text()).collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let ranges: Vec<_> = self
            .references
            .spans
            .iter()
            .rev()
            .filter(|span| {
                if span.file_id.is_none() || span.markdown.as_str() != markdown {
                    return false;
                }
                if protected.is_empty() {
                    return true;
                }
                let start = self
                    .references
                    .markdown_range(&self.text, 0..span.range.start)
                    .len();
                let end = start + markdown.len();
                !protected
                    .iter()
                    .any(|fragment| start < fragment.end && fragment.start < end)
            })
            .map(|span| span.range.clone())
            .collect();
        for range in ranges {
            self.erase_attachment_range(range, cx);
        }
    }

    /// Удаляет подпись в видимом тексте, сдвигая прежние позиции за ней.
    fn erase_attachment_range(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        let shift = |offset: usize| offset - offset.saturating_sub(range.start).min(range.len());
        let anchor = shift(self.anchor);
        let caret = shift(self.caret);
        let marked = self.marked.as_ref().and_then(|marked| {
            (marked.end <= range.start || marked.start >= range.end)
                .then(|| shift(marked.start)..shift(marked.end))
        });
        self.replace(range, "", cx);
        self.anchor = anchor;
        self.caret = caret;
        self.marked = marked;
    }

    /// Преобразует только принятые адреса текущего Workspace перед штатной вставкой из буфера.
    pub(super) fn paste_markup(&mut self, text: &str, cx: &mut Context<Self>) {
        let text = Zeroizing::new(self.references.convert_paste(text));
        self.replace_markup(self.input_range(None), &text, cx);
    }

    /// Исключает выделение, IME и уже вставленные ссылки из поиска по знаку у курсора.
    fn reference_query(&self) -> Option<Query<'_>> {
        if !self.references.suggestions_enabled
            || self.references.owner.is_none()
            || !self.selection().is_empty()
            || !self.can_format()
            || self.references.dismissed_at == Some((self.version, self.caret))
        {
            return None;
        }
        let query = query_at(&self.text, self.caret)?;
        if self.references.touches_span(&query.range) {
            return None;
        }
        let offset = if self.references.spans.is_empty() {
            query.range.start
        } else {
            self.references
                .markdown_range(&self.text, 0..query.range.start)
                .len()
        };
        workspace_reference_prose_at(self.text(), offset).then_some(query)
    }

    /// Возвращает кандидатов для последнего запроса в их порядке показа.
    fn reference_suggestions(&self) -> Vec<Suggestion> {
        self.reference_query().map_or_else(Vec::new, |query| {
            if query.people {
                self.references.mentioned_people(query.text)
            } else {
                self.references.conversations(query.text)
            }
        })
    }

    /// Удерживает Tab только при открытом выборе подсказок.
    pub(super) fn has_reference_suggestions(&self) -> bool {
        !self.reference_suggestions().is_empty()
    }

    /// Перемещает подсветку, пока открыт список. Иначе стрелка остаётся у текстового поля.
    pub(super) fn move_reference_selection(&mut self, down: bool, cx: &mut Context<Self>) -> bool {
        let len = self.reference_suggestions().len();
        if len == 0 {
            return false;
        }
        let previous = self.references.active.min(len - 1);
        self.references.active = if down {
            (previous + 1) % len
        } else {
            (previous + len - 1) % len
        };
        cx.notify();
        true
    }

    /// Закрывает список до очередного изменения текста или перемещения курсора.
    pub(super) fn dismiss_references(&mut self, cx: &mut Context<Self>) -> bool {
        if self.reference_query().is_none() {
            return false;
        }
        self.references.dismissed_at = Some((self.version, self.caret));
        self.references.active = 0;
        cx.notify();
        true
    }

    /// Выбирает подсвеченную подсказку и сообщает Enter, было ли действие обработано.
    pub(super) fn accept_reference(&mut self, silent: bool, cx: &mut Context<Self>) -> bool {
        let choices = self.reference_suggestions();
        let Some(item) = choices.get(self.references.active.min(choices.len().saturating_sub(1)))
        else {
            return false;
        };
        if silent && !matches!(item.target, WorkspaceReferenceTarget::User { .. }) {
            return false;
        }
        self.pick_reference(item, silent, cx)
    }

    /// Проверяет, что указанная мышью подсказка всё ещё относится к текущему запросу.
    fn pick_reference(
        &mut self,
        choice: &Suggestion,
        silent: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(query) = self.reference_query() else {
            return false;
        };
        if !self.reference_suggestions().contains(choice) {
            return false;
        }
        let mut target = choice.target.clone();
        if let WorkspaceReferenceTarget::User { silent: quiet, .. } = &mut target {
            *quiet = silent;
        }
        let Some(markdown) = build_workspace_reference_markdown(&choice.name, &target) else {
            return false;
        };
        self.replace_markup(query.range, &format!("{markdown} "), cx);
        true
    }

    /// Рисует подсказки над полем и оставляет каретку в тексте между открытиями.
    pub(super) fn render_reference_menu(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> Option<Deferred> {
        if !self.focus.is_focused(window) {
            return None;
        }
        let anchor = self.layout.as_ref()?.bounds;
        let suggestions = self.reference_suggestions();
        if suggestions.is_empty() {
            return None;
        }
        let palette = Palette::for_window(window, cx);
        let language = self.reference_language(cx);
        let active = self.references.active.min(suggestions.len() - 1);
        let mut surface = popup::menu_surface(
            "composer-reference-suggestions",
            palette,
            window.viewport_size(),
            px(440.),
            px(315.),
        )
        .debug_selector(|| "composer-reference-suggestions".into());
        for (index, choice) in suggestions.into_iter().enumerate() {
            let input = cx.entity().downgrade();
            let normal = choice.clone();
            let main = reference_option(
                ("composer-reference", index),
                choice.name.clone(),
                index == active,
                palette,
            )
            .debug_selector(move || format!("composer-reference-{index}"))
            .on_click(move |_, window, app| {
                let _ = input.update(app, |editor, cx| {
                    editor.pick_reference(&normal, false, cx);
                    editor.focus.focus(window);
                });
            })
            .min_w_0()
            .flex_1()
            .justify_start();
            let note = if choice.outside {
                Some(
                    language
                        .tr("desktop.messenger.composer.mentionOutside")
                        .to_owned(),
                )
            } else {
                choice.detail.clone()
            };
            let quiet = matches!(choice.target, WorkspaceReferenceTarget::User { .. }).then(|| {
                let input = cx.entity().downgrade();
                reference_option(
                    ("composer-mention-quiet", index),
                    language
                        .tr("desktop.messenger.composer.mentionQuiet")
                        .to_owned(),
                    false,
                    palette,
                )
                .debug_selector(move || format!("composer-mention-quiet-{index}"))
                .on_click(move |_, window, app| {
                    let _ = input.update(app, |editor, cx| {
                        editor.pick_reference(&choice, true, cx);
                        editor.focus.focus(window);
                    });
                })
            });
            surface = surface.child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .child(div().flex().min_w_0().child(main).children(quiet))
                    .children(
                        note.map(|text| label(palette, TextRole::Caption, text).truncate().px_2()),
                    ),
            );
        }
        Some(
            deferred(
                anchored()
                    .anchor(Corner::BottomLeft)
                    .position(point(anchor.left(), anchor.top() - px(8.)))
                    .snap_to_window_with_margin(px(4.))
                    .child(surface),
            )
            .with_priority(2),
        )
    }
}

/// Рисует пункт списка, который ведётся клавиатурой редактора и принимает клик мыши.
fn reference_option(
    id: impl Into<ElementId>,
    caption: String,
    active: bool,
    palette: Palette,
) -> Stateful<gpui::Div> {
    // У элемента нет собственного фокуса. Иначе нажатие мыши закрыло бы список до клика.
    div()
        .id(id)
        .h(px(32.))
        .px_2()
        .flex()
        .items_center()
        .rounded_md()
        .cursor_pointer()
        .when(active, |row| row.bg(palette.accent_soft))
        .hover(|style| style.bg(palette.card_hover))
        .child(label(palette, TextRole::Label, caption).truncate())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Сохраняет байты старых ссылок и снимает привязку только у изменённой подписи.
    #[test]
    fn restores_mentions_and_conversation_labels_with_their_original_markdown() {
        let id = Uuid::from_u128(42);
        let source = format!("[Лена](urn:user:{id}) и [Общий](urn:stream:{id})");
        let (mut text, spans) = References::default().display_markup(&source, Language::Ru);
        assert_eq!(text, "@Лена и Общий");
        let mut state = References::default();
        state.replaced(0..0, text.len(), &text, spans);
        assert_eq!(state.markdown(&text), source);
        text.replace_range(..1, "");
        state.replaced(0..1, 0, &text, Vec::new());
        assert_eq!(
            state.markdown(&text),
            format!("Лена и [Общий](urn:stream:{id})")
        );
    }

    /// Находит кириллический поиск, но не знак внутри почты или кода.
    #[test]
    fn uses_valid_query_boundaries() {
        let text = "Спроси @Ле";
        assert_eq!(
            query_at(text, text.len()).map(|query| query.text),
            Some("Ле")
        );
        assert!(query_at("mail@company", "mail@company".len()).is_none());
        assert!(query_at("`code @user", "`code @user".len()).is_none());
    }

    /// Выбирает обычное упоминание с клавиатуры, тихое мышью, затем сохраняет точные ссылки.
    #[cfg(feature = "gpui-test")]
    #[gpui::test]
    fn composer_picks_normal_and_quiet_mentions(cx: &mut gpui::TestAppContext) {
        use gpui::{Focusable, Modifiers, size};
        use workspace_core::context::AccountId;

        cx.update(Editor::bind_keys);
        let person = Uuid::from_u128(42);
        let (input, ui) = cx.add_window_view(|_, cx| {
            let mut editor = Editor::for_composer(cx);
            editor.set_reference_owner(OwnerContext {
                account_id: AccountId("reference-tests".into()),
                server: "https://workspace.test".into(),
                organization_id: "test-org".into(),
                project_id: "test-project".into(),
                user_id: Uuid::from_u128(3).to_string(),
            });
            editor.set_reference_suggestions(true, cx);
            editor.set_reference_people(
                vec![(
                    person,
                    UserProfile {
                        display_name: "Лена".into(),
                        email: None,
                        presence: None,
                        status_text: None,
                        status_emoji: None,
                        avatar: None,
                        created_at: None,
                    },
                )],
                Some(HashSet::new()),
                cx,
            );
            editor
        });
        ui.simulate_resize(size(px(650.), px(530.)));
        ui.update(|window, app| input.read(app).focus_handle(app).focus(window));
        ui.run_until_parked();

        ui.simulate_input("@Ле");
        ui.run_until_parked();
        assert!(ui.debug_bounds("composer-reference-suggestions").is_some());
        ui.simulate_keystrokes("enter");
        ui.run_until_parked();
        ui.read(|app| {
            let editor = input.read(app);
            assert_eq!(editor.text, "@Лена ");
            assert_eq!(editor.text(), format!("[Лена](urn:user:{person}) "));
        });

        ui.simulate_input("@Ле");
        ui.run_until_parked();
        let choice = ui
            .debug_bounds("composer-mention-quiet-0")
            .expect("тихое упоминание");
        ui.simulate_click(choice.center(), Modifiers::none());
        ui.run_until_parked();
        ui.read(|app| {
            let editor = input.read(app);
            assert_eq!(editor.text, "@Лена Лена ");
            assert_eq!(
                editor.text(),
                format!("[Лена](urn:user:{person}) [Лена](urn:user:{person} \"silent\") ")
            );
        });
    }

    /// Восстанавливает подпись вложения и сохраняет каноническую ссылку для черновика.
    #[test]
    fn restores_uploaded_attachment_without_showing_its_urn() {
        let id = Uuid::from_u128(66);
        let source = format!("Подпись\n\n![Кадр](urn:image:{id}?name=photo.png)");
        let (text, spans) = References::default().display_markup(&source, Language::Ru);
        assert_eq!(text, "Подпись\n\nphoto.png");
        assert_eq!(spans[0].file_id, Some(id));
        let mut state = References::default();
        state.replaced(0..0, text.len(), &text, spans);
        assert_eq!(state.markdown(&text), source);
    }

    /// Сохраняет подпись временного фото после восстановления текста с разметкой кода.
    #[test]
    fn temporary_photo_in_code_copies_only_its_label() {
        let id = Uuid::from_u128(77);
        let link = format!("![Кадр](urn:image:{id}?name=photo.png)");
        let source = format!("`{link}` здесь");
        let mut state = References::default();
        state.temporary_files.insert(
            id,
            TemporaryFile {
                markdown: Zeroizing::new(link),
                label: "photo.png".into(),
            },
        );
        let (text, spans) = state.display_markup(&source, Language::Ru);
        assert_eq!(text, "`photo.png` здесь");
        assert_eq!(References::display_offset(&spans, source.len()), text.len());
        state.replaced(0..0, text.len(), &text, spans);
        assert_eq!(state.markdown(&text), source);
        assert_eq!(state.clipboard_range(&text, 0..text.len()), text);
    }

    /// Превращает только ссылки на известную тему текущего аккаунта, сохраняя пунктуацию.
    #[test]
    fn converts_current_workspace_paste_to_a_readable_topic_link() {
        use workspace_core::{context::AccountId, messenger::catalog::Topic};

        let stream = Uuid::from_u128(51);
        let topic = Uuid::from_u128(52);
        let path =
            format!("/org/workspace.test/project/test-project/stream/{stream}/topic/{topic}");
        let state = References {
            owner: Some(OwnerContext {
                account_id: AccountId("reference-tests".into()),
                server: "https://workspace.test".into(),
                organization_id: "test-org".into(),
                project_id: "test-project".into(),
                user_id: Uuid::from_u128(3).to_string(),
            }),
            chats: Some(Arc::new([Chat {
                id: stream,
                name: "Канал".into(),
                direct: false,
                topics: vec![Topic {
                    id: topic,
                    name: "Тема".into(),
                    metadata: Default::default(),
                }],
                metadata: Default::default(),
            }])),
            ..References::default()
        };
        let old =
            format!("Здесь https://workspace.test{path}, отдельно https://elsewhere.test{path}");
        let new = state.convert_paste(&old);
        assert_eq!(
            new,
            format!(
                "Здесь [Канал › Тема](urn:topic:{topic}), отдельно https://elsewhere.test{path}"
            )
        );
        let suggestions = state.conversations("тем");
        assert!(
            matches!(suggestions.as_slice(), [topic_choice] if topic_choice.target == WorkspaceReferenceTarget::Topic { topic_id: topic, stream_id: None })
        );
    }
}
