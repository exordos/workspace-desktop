//! Переводит служебные части превью в Desktop и сохраняет строки автора как есть.

use uuid::Uuid;
use workspace_core::messenger::catalog::{
    Chat, MessagePreview, PreviewPart, SidebarKeyServiceKind, SidebarPreviewBody,
};
use workspace_core::storage::HistoryTime;

use super::preferences::Language;
use crate::platform::message_time::format_local_time;

impl Language {
    /// Показывает название личных заметок по собственной локали, сохраняя остальные имена.
    pub fn chat_title(self, chat: &Chat, user: Option<Uuid>) -> &str {
        if chat.direct && user.is_some() && user == chat.metadata.direct_peer_id {
            self.tr("activity.favorites")
        } else {
            &chat.name
        }
    }

    /// Показывает короткое превью на языке текущего аккаунта.
    pub fn preview_text(self, preview: &MessagePreview) -> String {
        render_preview(preview, self, |key| self.tr(key))
    }

    /// Добавляет подпись состояния ключа, когда каталог ещё не показывает обычный текст.
    pub fn sidebar_preview_text(self, body: &SidebarPreviewBody) -> String {
        let key = match body {
            SidebarPreviewBody::Summary(preview) => return self.preview_text(preview),
            SidebarPreviewBody::Encrypted { .. } => "desktop.preview.keyRequired",
            SidebarPreviewBody::KeyService(kind) => match kind {
                SidebarKeyServiceKind::Requested => "e2ee.keyRequestTitle",
                SidebarKeyServiceKind::Granted => "e2ee.serviceRequestGranted",
                SidebarKeyServiceKind::Rejected => "e2ee.keyRejectTitle",
                SidebarKeyServiceKind::Announced => "desktop.messenger.history.chatKeyUpdated",
            },
        };
        self.tr(key).to_owned()
    }
}

/// Собирает готовую строку из типизированных частей, используя каталог для служебных слов.
fn render_preview<'a>(
    preview: &'a MessagePreview,
    language: Language,
    label: impl Fn(&'static str) -> &'a str,
) -> String {
    let mut result = String::new();
    for part in preview.parts() {
        match part {
            PreviewPart::Text(text) => result.push_str(text),
            PreviewPart::Link => result.push_str(label("desktop.preview.link")),
            PreviewPart::ProtectedFragment => result.push_str(label("e2ee.secretFragment")),
            PreviewPart::Forwarded => result.push_str(label("message.forwarded")),
            PreviewPart::UnknownMention => {
                result.push('@');
                result.push_str(label("message.unknownMention"));
            }
            PreviewPart::Time(time) => {
                if let Some(time) = HistoryTime::parse(time) {
                    result.push_str(&format_local_time(&time, language.code()));
                }
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Сменой каталога можно сформировать третью локаль, сохранив совпадающий текст автора.
    #[test]
    fn generic_preview_renderer_translates_only_service_parts() {
        let preview =
            MessagePreview::plain_forward("Зашифрованный фрагмент [автор](urn:secret:v2:bad)");
        assert_eq!(
            Language::En.preview_text(&preview),
            "Зашифрованный фрагмент Encrypted fragment"
        );
        assert_eq!(
            render_preview(&preview, Language::En, |key| {
                assert_eq!(key, "e2ee.secretFragment");
                "Fragmento cifrado"
            }),
            "Зашифрованный фрагмент Fragmento cifrado",
        );
    }

    /// Распознаёт личные заметки по маршруту и сохраняет совпадающие имена обычных чатов.
    #[test]
    fn chat_title_localizes_only_the_matching_self_chat() {
        let user = Uuid::from_u128(1);
        let mut chat = Chat {
            id: Uuid::from_u128(2),
            name: "Избранное".into(),
            direct: true,
            topics: Vec::new(),
            metadata: workspace_core::messenger::catalog::ChatMetadata {
                direct_peer_id: Some(user),
                ..Default::default()
            },
        };
        assert_eq!(Language::En.chat_title(&chat, Some(user)), "Favorites");
        assert_eq!(Language::En.chat_title(&chat, None), "Избранное");
        chat.direct = false;
        assert_eq!(Language::En.chat_title(&chat, Some(user)), "Избранное");
    }
}
