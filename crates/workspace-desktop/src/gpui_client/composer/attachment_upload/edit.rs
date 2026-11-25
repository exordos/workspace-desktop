//! Выделяет уже опубликованные файлы при правке и загружает их превью с допуском истории.

use std::io::Read;

use gpui::{App, Context, Task};
use uuid::Uuid;
use workspace_core::{
    messenger::{
        files::MAX_ATTACHMENT_UPLOAD_BYTES, history::MediaAccess, references::MediaReference,
    },
    storage::media::MediaKind,
};
use zeroize::Zeroizing;

use super::{
    Composer, markup,
    presentation::{Card, CardId, CardPreview, CardStatus, decode_thumbnail},
};
use crate::gpui_client::contracts::AccountSession;

/// Существующие вложения из точной исходной версии правки.
#[derive(Default)]
pub(super) struct Existing {
    entries: Vec<Entry>, // Порядок показа: начальные, внутритекстовые, конечные.
    _preview_task: Option<Task<()>>, // Можно отменить после ухода из правки.
}

/// Одна карточка исходного сообщения, которое пользователь может отредактировать.
struct Entry {
    ordinal: usize,               // Устойчивая позиция, назначенная при открытии.
    reference: MediaReference,    // Имя, формат и UUID из исходной ссылки.
    markdown: Zeroizing<String>,  // Исходная ссылка для повторного сохранения.
    preview: Option<CardPreview>, // Уменьшенный кадр и его допуск после проверки Core.
}

impl Existing {
    /// Готовит карточки самостоятельных вложений и изображений внутри текста.
    fn extract(markdown: &str, protect_fragments: bool) -> (Self, String) {
        let references = markup::public_edit_media(markdown, protect_fragments);
        let lines = markdown
            .split_inclusive('\n')
            .scan(0, |start, line| {
                let from = *start;
                *start += line.len();
                Some((from, line.trim_end_matches(['\r', '\n'])))
            })
            .collect::<Vec<_>>();
        let mut prefix = 0;
        while let Some(&(offset, line)) = lines.get(prefix) {
            if !standalone(&references, offset, line) {
                break;
            }
            prefix += 1;
            // Старые нативные черновики отделяли каждое вложение пустой строкой.
            while lines
                .get(prefix)
                .is_some_and(|(_, line)| line.trim().is_empty())
            {
                prefix += 1;
            }
        }
        let mut suffix = lines.len();
        if let Some(last) = lines.iter().rposition(|(_, line)| !line.trim().is_empty()) {
            let tail = last + 1;
            let mut cursor = tail;
            while cursor > prefix {
                let (offset, line) = lines[cursor - 1];
                if !standalone(&references, offset, line) {
                    break;
                }
                cursor -= 1;
                while cursor > prefix && lines[cursor - 1].1.trim().is_empty() {
                    cursor -= 1;
                }
            }
            if cursor < tail {
                suffix = cursor;
            }
        }
        let start = lines.get(prefix).map_or(markdown.len(), |line| line.0);
        let end = lines
            .get(suffix)
            .map_or(markdown.len(), |line| line.0)
            .max(start);
        let selected = start..end;
        let mut entries = Vec::new();
        let mut unique = std::collections::HashSet::new();
        for reference in references {
            let isolated =
                reference.range.end <= selected.start || reference.range.start >= selected.end;
            let image = reference.image_syntax && reference.kind == MediaKind::Image;
            if (isolated || image) && unique.insert(&markdown[reference.range.clone()]) {
                entries.push(Entry {
                    ordinal: entries.len(),
                    markdown: Zeroizing::new(markdown[reference.range.clone()].to_owned()),
                    reference,
                    preview: None,
                });
            }
        }
        // Сохраняем исходные промежутки внутри тела, включая пустые строки и форматирование.
        (
            Self {
                entries,
                _preview_task: None,
            },
            markdown[selected].to_owned(),
        )
    }

    /// Возвращает, остались ли карточки опубликованного сообщения.
    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Возвращает снимок ссылок, оставленных пользователем при редактировании.
    pub(super) fn links(&self) -> impl Iterator<Item = (Uuid, &str)> {
        self.entries
            .iter()
            .map(|entry| (entry.reference.file_id, entry.markdown.as_str()))
    }

    /// Проверяет, соответствует ли нажатие карточке исходной версии.
    fn entry(&self, id: CardId) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.id() == id)
    }

    /// Возвращает точную ссылку, чтобы отличить разные подписи одного файла.
    pub(super) fn link_for(&self, id: CardId) -> Option<&str> {
        self.entry(id).map(|entry| entry.markdown.as_str())
    }

    /// Разрешает перетаскивать исходное изображение, если его превью доступно.
    pub(super) fn for_inline(&self, id: CardId) -> Option<(Uuid, &str)> {
        let entry = self.entry(id)?;
        (entry.preview.as_ref()?.available() && entry.reference.kind == MediaKind::Image)
            .then_some((entry.reference.file_id, entry.markdown.as_str()))
    }

    /// Закрывает запрос и убирает текстуры исходных изображений из окна.
    pub(super) fn release(&mut self, app: &mut App) {
        self._preview_task.take();
        for entry in self.entries.drain(..) {
            if let Some(preview) = entry.preview {
                preview.release(app);
            }
        }
    }
}

impl Entry {
    /// Связывает карточку правки с UUID сервера и порядком её создания.
    fn id(&self) -> CardId {
        CardId::Reference {
            id: self.reference.file_id,
            offset: self.ordinal,
        }
    }
}

/// Узнаёт строгую самостоятельную ссылку, сохраняя написанный рядом текст.
fn standalone(references: &[MediaReference], offset: usize, line: &str) -> bool {
    let line = offset..offset + line.len();
    references.iter().any(|reference| reference.range == line)
}

impl Composer {
    /// Готовит карточки и оставляет в редакторе текст правки и вписанные картинки.
    pub(in crate::gpui_client::composer) fn open_attachment_edit(
        &mut self,
        markdown: &str,
        cx: &mut Context<Self>,
    ) -> String {
        self.attachment_upload.edit.release(cx);
        let mixed = self
            .edit
            .as_ref()
            .is_some_and(|edit| edit.access.fragments().is_some());
        let (attachments, text) = Existing::extract(markdown, mixed);
        self.attachment_upload.edit = attachments;
        text
    }

    /// Отвязывает опубликованную карточку и возвращает точную ссылку для правки текста.
    pub(super) fn remove_existing_attachment(
        &mut self,
        id: CardId,
        cx: &mut Context<Self>,
    ) -> Option<Zeroizing<String>> {
        let index = self
            .attachment_upload
            .edit
            .entries
            .iter()
            .position(|entry| entry.id() == id)?;
        let entry = self.attachment_upload.edit.entries.remove(index);
        if let Some(preview) = entry.preview {
            preview.release(cx);
        }
        Some(entry.markdown)
    }

    /// Превращает ссылки исходного сообщения в карточки до карточек новой передачи.
    pub(super) fn existing_attachment_cards(
        &self,
        shown_inline: impl Fn(&str) -> bool,
        editable: bool,
    ) -> Vec<Card> {
        if self.edit.as_ref().is_none_or(|edit| !edit.access.current()) {
            return Vec::new();
        }
        self.attachment_upload
            .edit
            .entries
            .iter()
            .map(|entry| {
                let reference = &entry.reference;
                let image = reference.kind == MediaKind::Image;
                Card {
                    id: entry.id(),
                    name: reference
                        .name
                        .clone()
                        .unwrap_or_else(|| reference.label.clone()),
                    content_type: reference.metadata.content_type.clone(),
                    size: reference.metadata.size_bytes,
                    preview: entry.preview.clone(),
                    status: CardStatus::Ready,
                    can_remove: editable,
                    can_inline: image && editable,
                    in_text: image && shown_inline(entry.markdown.as_str()),
                }
            })
            .collect()
    }

    /// Загружает миниатюры только из подтверждённого оригинала редактируемого сообщения.
    pub(in crate::gpui_client::composer) fn load_attachment_edit_previews(
        &mut self,
        access: MediaAccess,
        cx: &mut Context<Self>,
    ) {
        let (Some(edit), Some(runtime)) = (&self.edit, self.draft_runtime.clone()) else {
            return;
        };
        let message = edit.expected.id;
        let address = self.address.clone();
        let protected = matches!(&edit.access, super::super::EditAccess::Whole(_));
        let files = self
            .attachment_upload
            .edit
            .entries
            .iter()
            .filter_map(|entry| {
                (entry.reference.kind == MediaKind::Image).then_some(entry.reference.file_id)
            })
            .collect::<std::collections::HashSet<_>>();
        if files.is_empty() {
            return;
        }
        let account = AccountSession {
            owner: address.route.owner.clone(),
            generation: address.generation,
        };
        let activation = runtime
            .message_encryption(&account)
            .map(|(_, activation)| activation);
        let task = cx.spawn(async move |entity, cx| {
            for file in files {
                let access = access.clone();
                let activation = activation.clone();
                let reply = runtime
                    .conversation(address.clone(), move |services, _| async move {
                        let mut media = if protected {
                            services
                                .messenger
                                .open_protected_media(
                                    &services.sync,
                                    &access,
                                    &activation?,
                                    message,
                                    file,
                                )
                                .await
                                .ok()?
                        } else {
                            services
                                .messenger
                                .open_media(&services.sync, &access, message, file)
                                .await
                                .ok()?
                        };
                        tokio::task::spawn_blocking(move || {
                            if media.size().ok()? > MAX_ATTACHMENT_UPLOAD_BYTES as u64 {
                                return None;
                            }
                            let mut bytes = Vec::new();
                            media
                                .by_ref()
                                .take(MAX_ATTACHMENT_UPLOAD_BYTES as u64 + 1)
                                .read_to_end(&mut bytes)
                                .ok()?;
                            if bytes.len() > MAX_ATTACHMENT_UPLOAD_BYTES {
                                return None;
                            }
                            let image = decode_thumbnail(&bytes)?;
                            let preview = if protected {
                                CardPreview::Protected {
                                    image,
                                    permit: media.protected_preview_permit()?,
                                }
                            } else {
                                CardPreview::Plain(image)
                            };
                            media.with_valid_access(|| preview).ok()
                        })
                        .await
                        .ok()
                        .flatten()
                    })
                    .await;
                let _ = entity.update(cx, |composer, cx| {
                    let Some(Ok(Some(preview))) = reply.for_current(Some(&composer.address)) else {
                        return;
                    };
                    if composer
                        .edit
                        .as_ref()
                        .is_none_or(|edit| edit.expected.id != message || !edit.access.current())
                    {
                        return;
                    }
                    for entry in composer
                        .attachment_upload
                        .edit
                        .entries
                        .iter_mut()
                        .filter(|entry| entry.reference.file_id == file)
                    {
                        entry.preview = Some(preview.clone());
                    }
                    cx.notify();
                });
            }
        });
        self.attachment_upload.edit._preview_task = Some(task);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workspace_core::messenger::references::media_references;

    /// Отличает отдельные файлы от изображения среди слов, сохраняя порядок карточек.
    #[test]
    fn edit_keeps_inline_text_and_extracts_end_cards() {
        let first = Uuid::from_u128(1);
        let second = Uuid::from_u128(2);
        let third = Uuid::from_u128(3);
        let leading = format!("[one](urn:file:{first}?name=one.txt)");
        let inline = format!("![two](urn:image:{second}?name=two.png)");
        let trailing = format!("[three](urn:video:{third}?name=three.mp4)");
        let markdown = format!("{leading}\n\nНачало {inline} конец\n\n{trailing}\n");
        let (existing, body) = Existing::extract(&markdown, false);
        assert_eq!(body, format!("Начало {inline} конец\n"));
        assert_eq!(
            existing.links().map(|(id, _)| id).collect::<Vec<_>>(),
            [first, second, third]
        );
        let submission =
            markup::with_attachments(&body, existing.links(), std::iter::empty(), true, false);
        assert_eq!(media_references(&submission).len(), 3);
    }

    /// Расшифрованная картинка остаётся внутри защищённых маркеров и не получает открытую карточку.
    #[test]
    fn mixed_edit_preserves_secret_image_and_removes_only_public_card() {
        let public = Uuid::from_u128(1);
        let protected = Uuid::from_u128(2);
        let public_link = format!("![public](urn:image:{public}?name=a.png)");
        let protected_link = format!("![private](urn:image:{protected}?name=b.png)");
        let secret = format!(":::\n{protected_link}\n:::");
        let original = format!("{public_link}\n\n{secret}");
        let (existing, body) = Existing::extract(&original, true);
        assert_eq!(body, secret);
        assert_eq!(
            existing.links().map(|(id, _)| id).collect::<Vec<_>>(),
            [public]
        );
    }
}
