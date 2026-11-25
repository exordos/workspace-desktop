//! Включает защиту выделения в обычной теме и хранит выбор нового ключа для одной отправки.

use std::sync::Arc;

use gpui::{AnyElement, Context, Div, FocusHandle, IntoElement, Stateful, div, prelude::*};
use tokio::sync::broadcast::error::RecvError;
use uuid::Uuid;
use workspace_core::{
    messenger::{
        encryption::{
            EncryptionActivation, MessageEncryption,
            fragments::{FragmentEdit, has_protected_draft},
        },
        history::MediaAccess,
    },
    storage::{HistoryMessage, PlaintextDraft},
};

use super::{
    Composer, Destination, EditAccess, ProtectedEdit, WriteFailure, editor::Format,
    toolbar::icon_button,
};
use crate::gpui_client::{
    preferences::Language,
    ui::{Button, ButtonAppearance, Icon, Palette},
};

/// Проверяет и основной ввод, и все ответы перед передачей снимка в фоновую запись.
pub(super) fn is_protected_draft(draft: &PlaintextDraft) -> bool {
    has_protected_draft(&draft.markdown)
        || draft.replies.as_ref().is_some_and(|replies| {
            replies.tabs.iter().any(|tab| {
                has_protected_draft(&tab.answer)
                    || tab.fragment.as_deref().is_some_and(has_protected_draft)
            })
        })
}

impl Composer {
    /// Возвращает точную обычную тему, пока поле может открыть правку.
    pub(in crate::gpui_client) fn fragment_edit_topic(&self) -> Option<Uuid> {
        match self.destination {
            Destination::Ready {
                topic,
                encrypted: false,
                ..
            } if self.writing.is_none()
                && self.edit.is_none()
                && self
                    .plain_target
                    .as_ref()
                    .is_some_and(|target| target.fragments_allowed()) =>
            {
                Some(topic)
            }
            _ => None,
        }
    }

    /// Открывает правку только в памяти и сразу подписывает её на отзыв любого исходного ключа.
    pub(in crate::gpui_client) fn open_fragment_edit(
        &mut self,
        expected: HistoryMessage,
        access: FragmentEdit,
        engine: Arc<MessageEncryption>,
        activation: EncryptionActivation,
        media_access: Option<MediaAccess>,
        cx: &mut Context<Self>,
    ) {
        if self.fragment_edit_topic() != Some(expected.topic_id) {
            return;
        }
        let mut changes = engine.subscribe();
        let Some(text) = access.with_markdown(|text| zeroize::Zeroizing::new(text.to_owned()))
        else {
            return;
        };
        self.discard_attachments(cx);
        let access = Arc::new(access);
        let expected_id = expected.id;
        let watching = access.clone();
        let watch = cx.spawn(async move |entity, cx| {
            loop {
                match changes.recv().await {
                    Ok(change) if change.activation == activation => {}
                    Ok(_) => continue,
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => return,
                }
                if watching.with_markdown(|_| ()).is_some() {
                    continue;
                }
                let _ = entity.update(cx, |composer, cx| {
                    composer.cancel_protected_edit(Some(expected_id), cx)
                });
                return;
            }
        });
        self.edit = Some(ProtectedEdit {
            expected,
            access: EditAccess::Fragments(access.clone()),
            draft: zeroize::Zeroizing::new(self.editor.read(cx).text().to_owned()),
            draft_replies: Some(std::mem::take(&mut self.replies)),
            _watch: Some(watch),
        });
        let text = zeroize::Zeroizing::new(self.open_attachment_edit(&text, cx));
        self.editor.update(cx, |editor, cx| {
            editor.show_fragment_edit(&text, access, cx)
        });
        if let Some(access) = media_access {
            self.load_attachment_edit_previews(access, cx);
        }
        cx.notify();
    }

    /// Объясняет, что для правки не удалось открыть все исходные фрагменты.
    pub(in crate::gpui_client) fn fragment_edit_unavailable(&mut self, cx: &mut Context<Self>) {
        self.write_failure = Some((
            self.editor.read(cx).version(),
            self.replies.revision,
            WriteFailure::Edit,
        ));
        cx.notify();
    }

    /// Убирает только правку, которая потеряла доступ к одному из своих исходных чатов.
    pub(in crate::gpui_client) fn close_revoked_fragment_edit(&mut self, cx: &mut Context<Self>) {
        if self.edit.as_ref().is_some_and(|edit| {
            matches!(&edit.access, EditAccess::Fragments(_)) && !edit.access.current()
        }) {
            self.cancel_protected_edit(None, cx);
        }
    }

    /// Добавляет команду шифрования выделения только в подтверждённый обычный редактор.
    pub(super) fn fragment_toolbar(
        &self,
        palette: Palette,
        locale: Language,
        can_format: bool,
        focus: &FocusHandle,
        cx: &Context<Self>,
    ) -> Option<Stateful<Div>> {
        if self
            .plain_target
            .as_ref()
            .is_none_or(|target| !target.fragments_allowed())
            || !matches!(
                self.destination,
                Destination::Ready {
                    encrypted: false,
                    ..
                }
            )
        {
            return None;
        }
        let target = cx.entity().downgrade();
        Some(icon_button(
            "composer-format-encrypted-spoiler",
            locale.tr("desktop.messenger.composer.protectBeforeTyping"),
            (Icon::EncryptedSpoiler, [32.; 2]),
            palette,
            focus,
            !can_format,
            move |window, cx| {
                let _ = target.update(cx, |composer, cx| {
                    composer.editor.update(cx, |editor, cx| {
                        editor.apply_format(Format::EncryptedSpoiler, cx)
                    });
                    composer.persist_draft(cx);
                    composer.focus_editor(window, cx);
                });
            },
        ))
    }

    /// Предлагает новый ключ только для сообщения с защищённым вводом.
    pub(super) fn fragment_key_option(
        &self,
        palette: Palette,
        locale: Language,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let editor = self.editor.read(cx);
        if self.edit.is_some()
            || self
                .plain_target
                .as_ref()
                .is_none_or(|target| !target.fragments_allowed())
            || !matches!(
                self.destination,
                Destination::Ready {
                    encrypted: false,
                    ..
                }
            )
            || !has_protected_draft(&self.replies.markdown(editor.text()))
        {
            return None;
        }
        let target = cx.entity().downgrade();
        let mut option = Button::new(
            "composer-new-fragment-key",
            locale.tr("composer.createSecretKey"),
        );
        option.appearance = if self.fragment_new_key {
            ButtonAppearance::Neutral
        } else {
            ButtonAppearance::Ghost
        };
        option.icon = self.fragment_new_key.then_some(Icon::Check);
        Some(
            div()
                .flex()
                .child(option.render(palette, None, move |_, cx| {
                    let _ = target.update(cx, |composer, cx| {
                        composer.fragment_new_key = !composer.fragment_new_key;
                        cx.notify();
                    });
                }))
                .into_any_element(),
        )
    }
}
