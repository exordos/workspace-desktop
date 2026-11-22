//! Хранит вкладки ответов открытого разговора и показывает их над полем ввода.

mod draft;

use std::{borrow::Cow, cell::Cell, rc::Rc};

use gpui::{Context, Div, FocusHandle, Pixels, ScrollHandle, Window, div, prelude::*, px};
use uuid::Uuid;
use workspace_core::messenger::quote;

use super::Composer;
use crate::gpui_client::ui::{Button, ButtonAppearance, Palette, quote_preview};

mod drag;

use drag::ActiveDrag;

/// Действие над источником ответа.
#[derive(Clone, Copy)]
pub(in crate::gpui_client) enum ReplyAction {
    Replace, // Начать ответ или сменить источник в активной вкладке.
    Add,     // Создать отдельную вкладку, даже если источник уже процитирован.
}

/// Снимок подтверждённого сообщения, выбранного в текущем разговоре.
pub(in crate::gpui_client) struct ReplyQuote {
    pub id: Uuid,                 // Публичный UUID исходной строки.
    pub author: String,           // Подпись автора из текущей истории.
    pub preview: String,          // Фрагмент или краткий текст исходной строки.
    pub fragment: Option<String>, // Выделенный фрагмент для параметра ссылки.
}

/// Одна вкладка с отдельным ответом.
struct Tab {
    id: Uuid,                  // Локальный ID для кнопок, сохраняется при смене и перестановке.
    quote: ReplyQuote,         // Источник текущего ответа.
    creation: u64,             // Порядок создания, который не меняется при перестановке.
    restored: bool,            // Превью нужно заново сверять с показанным окном.
    answer: String,            // Ввод, сохранённый при последнем уходе с вкладки.
    focus: FocusHandle,        // Постоянный фокус кнопки выбора.
    remove_focus: FocusHandle, // Отдельный фокус кнопки удаления.
}

/// Ответы и отложенный обычный черновик одного открытого разговора.
#[derive(Default)]
pub(super) struct Replies {
    tabs: Vec<Tab>,           // Порядок сборки итогового сообщения.
    active: usize,            // Индекс поля, которое сейчас редактируется. Ноль при пустом списке.
    plain: String,            // Черновик до добавления первой независимой вкладки.
    pub revision: u64,        // Меняется при каждом изменении состава, порядка или выбора.
    scroll: ScrollHandle,     // Прокрутка пары «вкладка и удаление» как одного элемента.
    drag: Option<ActiveDrag>, // Источник текущего перетаскивания, если оно началось здесь.
    last_reveal: Rc<Cell<Option<(usize, u64, Pixels)>>>, // Последняя позиция, версия и ширина ряда.
}

impl Replies {
    /// Проверяет, есть ли ответ, включая ещё не сохранённый текст активной вкладки.
    pub(super) fn has_answer(&self, current: &str) -> bool {
        !current.trim().is_empty()
            || self
                .tabs
                .iter()
                .enumerate()
                .any(|(i, tab)| i != self.active && !tab.answer.trim().is_empty())
    }

    /// Возвращает обычный ввод или снимок отдельных цитат вместе с ответами.
    pub(super) fn markdown<'a>(&self, current: &'a str) -> Cow<'a, str> {
        if self.tabs.is_empty() {
            return Cow::Borrowed(current);
        }
        let mut markdown = String::new();
        for (index, tab) in self.tabs.iter().enumerate() {
            if index > 0 {
                markdown.push_str("\n\n");
            }
            markdown.push_str(&quote::reference(
                tab.quote.id,
                &tab.quote.author,
                tab.quote.fragment.as_deref(),
            ));
            let answer = if index == self.active {
                current
            } else {
                &tab.answer
            };
            if !answer.trim().is_empty() {
                markdown.push_str("\n\n");
                markdown.push_str(answer.trim());
            }
        }
        Cow::Owned(markdown)
    }

    /// Снимает отдельный список проверяемых источников сразу при нажатии «Отправить».
    pub(super) fn sources(&self) -> Vec<quote::Source> {
        self.tabs
            .iter()
            .map(|tab| quote::Source {
                message_id: tab.quote.id,
                fragment: tab.quote.fragment.clone(),
            })
            .collect()
    }

    /// Запоминает текущий текст, прежде чем действие сможет сменить отображаемую вкладку.
    fn save_active(&mut self, current: &str) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.answer = current.to_owned();
        }
    }

    /// Увеличивает версию снимка и возвращает текст, который следует показать.
    fn change(&mut self) -> String {
        self.revision = self.revision.wrapping_add(1);
        self.tabs
            .get(self.active)
            .map_or_else(|| self.plain.clone(), |tab| tab.answer.clone())
    }

    /// Начинает ответ, заменяет текущую цитату или добавляет ещё одну вкладку.
    fn quote(
        &mut self,
        action: ReplyAction,
        quote: ReplyQuote,
        current: &str,
        cx: &Context<Composer>,
    ) -> String {
        self.save_active(current);
        match action {
            ReplyAction::Replace if !self.tabs.is_empty() => {
                let tab = &mut self.tabs[self.active];
                tab.quote = quote;
                tab.restored = false;
            }
            action => {
                let answer = match action {
                    ReplyAction::Replace => current.to_owned(),
                    ReplyAction::Add => {
                        if self.tabs.is_empty() {
                            self.plain = current.to_owned();
                        }
                        String::new()
                    }
                };
                self.tabs.push(Tab {
                    id: Uuid::new_v4(),
                    quote,
                    answer,
                    creation: self.revision,
                    restored: false,
                    focus: cx.focus_handle(),
                    remove_focus: cx.focus_handle(),
                });
                self.active = self.tabs.len() - 1;
            }
        }
        self.change()
    }

    /// Выбирает вкладку и сохраняет текст прежнего ответа.
    fn select(&mut self, id: Uuid, current: &str) -> Option<String> {
        let index = self.index(id)?;
        if self.active == index {
            return None;
        }
        self.save_active(current);
        self.active = index;
        Some(self.change())
    }

    /// Убирает вкладку и выбирает соседнюю, а при выходе возвращает обычный черновик.
    fn remove(&mut self, id: Uuid, current: &str) -> Option<String> {
        let index = self.index(id)?;
        self.save_active(current);
        let removed = self.tabs.remove(index);
        if self.tabs.is_empty() {
            if self.plain.is_empty() {
                self.plain = removed.answer;
            }
            let text = self.change();
            self.plain.clear();
            return Some(text);
        }
        self.active = if self.active > index {
            self.active - 1
        } else {
            self.active.min(self.tabs.len() - 1)
        };
        Some(self.change())
    }

    /// Закрывает всю сессию ответов и восстанавливает обычный черновик.
    fn cancel(&mut self, current: &str) -> Option<String> {
        self.save_active(current);
        let first = self.tabs.iter().min_by_key(|tab| tab.creation)?;
        let draft = if self.plain.is_empty() {
            first.answer.clone()
        } else {
            std::mem::take(&mut self.plain)
        };
        self.clear();
        Some(draft)
    }

    /// Переставляет вкладку по позиции вставки, сохраняя активный ответ и его текст.
    fn reorder(&mut self, id: Uuid, insertion: usize) -> bool {
        let Some(source) = self.index(id) else {
            return false;
        };
        let insertion = insertion.min(self.tabs.len());
        let destination = insertion - usize::from(insertion > source);
        if destination == source {
            return false;
        }
        let active = self.tabs[self.active].id;
        let moved = self.tabs.remove(source);
        self.tabs.insert(destination, moved);
        self.active = self
            .index(active)
            .expect("активная вкладка осталась в списке");
        self.revision = self.revision.wrapping_add(1);
        true
    }

    /// Отдаёт приоритет паре с клавиатурным фокусом, иначе выбирает активный ответ.
    fn reveal_index(&self, window: &Window) -> usize {
        self.tabs
            .iter()
            .position(|tab| tab.focus.is_focused(window) || tab.remove_focus.is_focused(window))
            .unwrap_or(self.active)
    }

    /// Возвращает позицию той же вкладки после предыдущих операций.
    fn index(&self, id: Uuid) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.id == id)
    }

    /// Убирает ровно тот снимок ответов, который уже попал в очередь.
    pub(super) fn clear(&mut self) {
        self.tabs.clear();
        self.last_reveal.set(None);
        self.active = 0;
        self.plain.clear();
        self.revision = self.revision.wrapping_add(1);
    }
}

/// Кнопка управления вкладками ответа.
#[derive(Clone, Copy)]
enum TabAction {
    Select(Uuid), // Показать конкретную вкладку.
    Remove(Uuid), // Удалить конкретную вкладку.
    Clear,        // Закрыть все вкладки и вернуться к обычному вводу.
}

impl Composer {
    /// Ставит цитату в текущий доступный редактор и возвращает фокус ответа.
    pub(in crate::gpui_client) fn reply(
        &mut self,
        action: ReplyAction,
        quote: ReplyQuote,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = self.editor.read(cx);
        if !self.reply_available() || editor.composing() {
            return;
        }
        let text = self.replies.quote(action, quote, editor.text(), cx);
        self.show_reply_text(text, true, window, cx);
    }

    /// Проверяет доступность цитирования для подтверждённой обычной темы.
    pub(in crate::gpui_client) fn reply_available(&self) -> bool {
        matches!(self.destination, super::Destination::Ready { .. })
    }

    /// Проверяет, можно ли изменить вкладки, сохранив системную композицию нетронутой.
    pub(in crate::gpui_client) fn reply_action_enabled(&self, app: &gpui::App) -> bool {
        self.reply_available() && !self.editor.read(app).composing()
    }

    /// Применяет действие к живой вкладке и показывает её редактор.
    fn reply_tab(
        &mut self,
        action: TabAction,
        mouse: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = self.editor.read(cx);
        if editor.composing() {
            return;
        }
        let current = editor.text();
        let text = match action {
            TabAction::Select(id) => self.replies.select(id, current),
            TabAction::Remove(id) => self.replies.remove(id, current),
            TabAction::Clear => self.replies.cancel(current),
        };
        if let Some(text) = text {
            self.show_reply_text(
                text,
                mouse || matches!(action, TabAction::Remove(_) | TabAction::Clear),
                window,
                cx,
            );
        }
    }

    /// Возвращает текст нужной вкладки без вмешательства в системную композицию.
    fn show_reply_text(
        &mut self,
        text: String,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor
            .update(cx, |editor, cx| editor.show_reply(&text, cx));
        self.persist_draft(cx);
        if focus {
            self.focus_editor(window, cx);
        }
        cx.notify();
    }

    /// Рисует переключатели и активный предпросмотр над полем.
    pub(super) fn render_replies(
        &self,
        palette: Palette,
        window: &Window,
        cx: &Context<Self>,
    ) -> Option<Div> {
        let current = self.replies.tabs.get(self.replies.active)?;
        let multiple = self.replies.tabs.len() > 1;
        let reveal_index = self.replies.reveal_index(window);
        let revision = self.replies.revision;
        let dragged = self.active_reply_drag(cx);
        let scroll = self.replies.scroll.clone();
        let last_reveal = self.replies.last_reveal.clone();
        let target = cx.entity().downgrade();
        let bar = div()
            .on_children_prepainted(move |_, window, app| {
                let next = Some((reveal_index, revision, scroll.bounds().size.width));
                if dragged.is_none() && last_reveal.replace(next) != next {
                    // При сужении фактическая ширина появляется только после размещения ряда.
                    let target = target.clone();
                    window.defer(app, move |window, app| {
                        let _ = target.update(app, |composer, _| {
                            if composer.replies.tabs.len() > 1
                                && composer.replies.last_reveal.get() == next
                            {
                                let index = composer.replies.reveal_index(window);
                                composer.replies.scroll.scroll_to_item(index);
                                window.refresh();
                            }
                        });
                    });
                }
            })
            .id("composer-reply-tabs")
            .debug_selector(|| "composer-reply-tabs".into())
            .flex()
            .items_center()
            .min_w_0()
            .gap_1()
            .overflow_x_scroll()
            .track_scroll(&self.replies.scroll)
            .children(self.replies.tabs.iter().enumerate().map(|(index, tab)| {
                div()
                    .flex()
                    .items_center()
                    .flex_none()
                    .when(dragged == Some(tab.id), |tab| tab.opacity(0.6))
                    .rounded_md()
                    .border_1()
                    .border_color(palette.border)
                    .bg(if index == self.replies.active {
                        palette.card_selected
                    } else {
                        palette.surface
                    })
                    .child(
                        self.reply_button(
                            (gpui::ElementId::Uuid(tab.id), "reply-tab"),
                            tab.quote.author.clone(),
                            TabAction::Select(tab.id),
                            Some(&tab.focus),
                            palette,
                            cx,
                        )
                        .max_w(px(180.))
                        .map(|button| self.reply_reorder_key(button, tab.id, palette, cx))
                        .map(|button| self.reply_drag_handle(button, tab, palette, cx))
                        .debug_selector(move || format!("composer-reply-tab-{index}")),
                    )
                    .child(
                        self.reply_button(
                            (gpui::ElementId::Uuid(tab.id), "reply-remove"),
                            "×",
                            TabAction::Remove(tab.id),
                            Some(&tab.remove_focus),
                            palette,
                            cx,
                        )
                        .debug_selector(move || format!("composer-reply-remove-{index}")),
                    )
            }));
        let strip = div()
            .relative()
            .flex_1()
            .min_w_0()
            .child(self.reply_drag_target(bar.w_full(), cx))
            .children(
                dragged
                    .and_then(|_| self.replies.drop_slot(window.mouse_position()))
                    .map(|slot| slot.indicator(palette)),
            );
        let preview = quote_preview(
            palette,
            format!("Ответить: {}", current.quote.author),
            current.quote.preview.clone(),
        )
        .flex_1()
        .debug_selector(|| "composer-reply-preview".into());
        let close = self
            .reply_button(
                "composer-reply-clear",
                "×",
                TabAction::Clear,
                None,
                palette,
                cx,
            )
            .debug_selector(|| "composer-reply-clear".into());
        let panel = div().flex().flex_col().min_w_0().gap_1();
        Some(if multiple {
            panel
                .child(
                    div()
                        .flex()
                        .min_w_0()
                        .items_start()
                        .child(strip)
                        .child(close),
                )
                .child(preview)
        } else {
            panel.child(
                div()
                    .flex()
                    .min_w_0()
                    .items_start()
                    .child(preview)
                    .child(close),
            )
        })
    }

    /// Переставляет активную вкладку с клавиатуры без новых кнопок в ряду.
    fn reply_reorder_key(
        &self,
        button: gpui::Stateful<Div>,
        id: Uuid,
        palette: Palette,
        cx: &Context<Self>,
    ) -> gpui::Stateful<Div> {
        let target = cx.entity().downgrade();
        button
            .tooltip(move |_, app| {
                crate::gpui_client::ui::popup::tooltip(
                    palette,
                    "Shift+←/→ — переставить активный ответ".into(),
                    app,
                )
            })
            .on_key_down(move |event, _, app| {
                let stroke = &event.keystroke;
                if !stroke.modifiers.shift
                    || stroke.modifiers.control
                    || stroke.modifiers.platform
                    || stroke.modifiers.alt
                {
                    return;
                }
                let right = match stroke.key.as_str() {
                    "right" => true,
                    "left" => false,
                    _ => return,
                };
                let _ = target.update(app, |composer, cx| {
                    if composer
                        .replies
                        .tabs
                        .get(composer.replies.active)
                        .is_some_and(|tab| tab.id == id)
                    {
                        let insertion = if right {
                            composer.replies.active + 2
                        } else {
                            composer.replies.active.saturating_sub(1)
                        };
                        composer.move_reply(id, insertion, cx);
                    }
                });
                app.stop_propagation();
            })
    }

    /// Записывает новый порядок без перезаписи текста и каретки активного ответа.
    fn move_reply(&mut self, id: Uuid, insertion: usize, cx: &mut Context<Self>) -> bool {
        if self.editor.read(cx).composing() || !self.replies.reorder(id, insertion) {
            return false;
        }
        self.persist_draft(cx);
        cx.notify();
        true
    }

    /// Включает штатное управление кнопками мышью и клавиатурой.
    fn reply_button(
        &self,
        id: impl Into<gpui::ElementId>,
        label: impl Into<gpui::SharedString>,
        action: TabAction,
        focus: Option<&FocusHandle>,
        palette: Palette,
        cx: &Context<Self>,
    ) -> gpui::Stateful<Div> {
        let mut button = Button::new(id, label);
        button.appearance = ButtonAppearance::Ghost;
        button.disabled = self.editor.read(cx).composing();
        let target = cx.entity().downgrade();
        button.render_event(palette, focus, move |event, window, app| {
            let mouse = event.mouse_position().is_some();
            let _ = target.update(app, |composer, cx| {
                composer.reply_tab(action, mouse, window, cx)
            });
        })
    }
}
