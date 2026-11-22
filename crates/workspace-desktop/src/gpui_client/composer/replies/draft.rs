//! Переводит вкладки в хранимый снимок и обратно. При восстановлении
//! подробности источника берём только из подтверждённого окна истории.

use gpui::Context;
use uuid::Uuid;
use workspace_core::storage::{DraftReplies, DraftReply, PlaintextDraft};

use super::{Composer, Replies, ReplyQuote, Tab};

impl Replies {
    /// Снимает видимый порядок и берёт последний ввод активной вкладки прямо из поля.
    pub(in crate::gpui_client::composer) fn draft_snapshot(
        &self,
        editor_text: &str,
    ) -> (String, Option<DraftReplies>) {
        let Some(active) = self.tabs.get(self.active) else {
            return (editor_text.to_owned(), None);
        };
        (
            self.plain.clone(),
            Some(DraftReplies {
                active: active.id,
                tabs: self
                    .tabs
                    .iter()
                    .map(|tab| DraftReply {
                        id: tab.id,
                        message_id: tab.quote.id,
                        fragment: tab.quote.fragment.clone(),
                        answer: if tab.id == active.id {
                            editor_text.to_owned()
                        } else {
                            tab.answer.clone()
                        },
                        creation: tab.creation,
                    })
                    .collect(),
            }),
        )
    }

    /// Собирает новые фокусы вокруг прежнего порядка, вкладки и исходного текста.
    pub(in crate::gpui_client::composer) fn restore_draft(
        &mut self,
        saved: PlaintextDraft,
        cx: &mut Context<Composer>,
    ) -> String {
        let Some(saved_replies) = saved.replies else {
            return saved.markdown;
        };
        let active = saved_replies.active;
        self.plain = saved.markdown;
        self.tabs = saved_replies
            .tabs
            .into_iter()
            .map(|saved| Tab {
                id: saved.id,
                quote: ReplyQuote {
                    id: saved.message_id,
                    author: "Сообщение".into(),
                    preview: "Источник недоступен".into(),
                    fragment: saved.fragment,
                },
                creation: saved.creation,
                restored: true,
                answer: saved.answer,
                focus: cx.focus_handle(),
                remove_focus: cx.focus_handle(),
            })
            .collect();
        if let Some(latest) = self.tabs.iter().map(|tab| tab.creation).max() {
            // Новая вкладка должна быть новее каждой восстановленной, даже после удаления первых.
            self.revision = self.revision.max(latest);
        }
        self.active = self.index(active).unwrap_or(0);
        self.change()
    }
}

impl Composer {
    /// Подставляет сведения из доступной строки, сохраняя ссылку и набранный ответ.
    pub(in crate::gpui_client) fn refresh_draft_quotes(
        &mut self,
        quote_source: impl Fn(Uuid, Option<&str>) -> Option<ReplyQuote>,
        cx: &mut Context<Self>,
    ) {
        let mut changed = false;
        for tab in self.replies.tabs.iter_mut().filter(|tab| tab.restored) {
            let quote = quote_source(tab.quote.id, tab.quote.fragment.as_deref());
            let (author, preview) = quote.map_or_else(
                || ("Сообщение".into(), "Источник недоступен".into()),
                |quote| (quote.author, quote.preview),
            );
            if (tab.quote.author.as_str(), tab.quote.preview.as_str()) != (&author, &preview) {
                tab.quote.author = author;
                tab.quote.preview = preview;
                changed = true;
            }
        }
        if changed {
            cx.notify();
        }
    }
}
