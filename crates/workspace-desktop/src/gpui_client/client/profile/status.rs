//! Редактирует личный статус в модалке и показывает набор эмодзи с поиском.

use gpui::{
    App, Context, Div, Entity, FocusHandle, SharedString, Task, WeakEntity, Window, div,
    prelude::*, px,
};
use workspace_core::{
    directory::{ProfilePresence, UserProfile},
    messenger::own_profile::StatusEdit,
};

use super::Profile;
use crate::gpui_client::{
    contracts::AccountSession,
    preferences::Preferences,
    right_panel::RightPanel,
    runtime::RuntimeClient,
    ui::{
        Button, ButtonAppearance, Palette, TextInput, TextInputChanged, TextRole, activate,
        animation::Motion, checkbox::Checkbox, label,
    },
};

const PRESETS: [&str; 6] = ["💬", "🏠", "🌴", "🍽️", "⛑️", "🗓️"];
const EMOJIS: &str = include_str!("../../../../assets/status-emojis.tsv");
const CATEGORIES: [(&str, &str); 8] = [
    ("smileys_people", "desktop.status.emoji.smileys"),
    ("animals_nature", "desktop.status.emoji.animals"),
    ("food_drink", "desktop.status.emoji.food"),
    ("travel_places", "desktop.status.emoji.travel"),
    ("activities", "desktop.status.emoji.activities"),
    ("objects", "desktop.status.emoji.objects"),
    ("symbols", "desktop.status.emoji.symbols"),
    ("flags", "desktop.status.emoji.flags"),
];

/// До чтения прежнего статуса сохранять обнулённые поля нельзя.
#[derive(Clone, Copy, PartialEq, Eq)]
enum InitialStatus {
    Loading, // Ожидается ответ справочника.
    Ready,   // Поля заполнены актуальным снимком.
    Failed,  // Пользователь может повторить загрузку.
}

/// Управляет полями и единственным запросом модального окна статуса.
pub(super) struct Status {
    root: WeakEntity<Profile>,    // Панель, которая покажет успешный ответ.
    host: WeakEntity<RightPanel>, // Оверлей и возврат фокуса.
    session: AccountSession,      // Поколение, которому принадлежит статус.
    runtime: RuntimeClient,       // Фоновый вызов Workspace API.
    text: Entity<TextInput>,      // Поле статуса с системным вводом.
    search: Entity<TextInput>,    // Поиск открытого набора эмодзи.
    emoji: String,                // Выбранный знак, пустой очищает прежний.
    away: bool,                   // Явное отсутствие по выбору пользователя.
    picker: bool,                 // Встроенный набор раскрыт между строками формы.
    category: usize,              // Номер видимой категории при пустом поиске.
    save: Option<Task<()>>,       // Активный запрос до показа ответа.
    failed: bool,                 // Ошибка сохраняет введённые данные.
    initial: FocusHandle,         // Первый пресет получает фокус при входе.
    first_frame: bool,            // Передаёт фокус один раз после открытия формы.
    loaded: InitialStatus,        // Первоначальное чтение, если корень ещё не получил профиль.
}

impl Status {
    /// Создаёт форму по текущему подтверждённому статусу пользователя.
    pub(super) fn new(
        root: WeakEntity<Profile>,
        host: WeakEntity<RightPanel>,
        session: AccountSession,
        runtime: RuntimeClient,
        initial: Option<UserProfile>,
        away: Option<bool>,
        cx: &mut Context<Self>,
    ) -> Self {
        let language = Preferences::read(cx).language;
        let text = cx.new(|cx| {
            let mut input = TextInput::new(
                "profile-status-text",
                language.tr("settings.statusPlaceholder"),
                false,
                cx,
            )
            .unframed();
            input.set_text(
                initial
                    .as_ref()
                    .and_then(|profile| profile.status_text.as_deref())
                    .unwrap_or_default(),
                cx,
            );
            input
        });
        let search = cx.new(|cx| {
            TextInput::new(
                "profile-status-emoji-search",
                language.tr("desktop.status.emojiSearch"),
                false,
                cx,
            )
        });
        cx.subscribe(&search, |_, _, _: &TextInputChanged, cx| cx.notify())
            .detach();
        let needs_load = initial.is_none();
        let mut status = Self {
            root,
            host,
            session,
            runtime,
            text,
            search,
            away: away == Some(true)
                || initial
                    .as_ref()
                    .is_some_and(|profile| profile.presence == Some(ProfilePresence::DoNotDisturb)),
            emoji: initial
                .and_then(|profile| profile.status_emoji)
                .unwrap_or_default(),
            picker: false,
            category: 0,
            save: None,
            failed: false,
            initial: cx.focus_handle(),
            first_frame: true,
            loaded: if needs_load {
                InitialStatus::Loading
            } else {
                InitialStatus::Ready
            },
        };
        if needs_load {
            status.load_initial(cx);
        }
        status
    }

    /// Получает исходные значения для безопасного изменения любого отдельного поля.
    fn load_initial(&mut self, cx: &mut Context<Self>) {
        self.loaded = InitialStatus::Loading;
        self.text
            .update(cx, |input, cx| input.set_enabled(false, cx));
        let runtime = self.runtime.clone();
        let session = self.session.clone();
        cx.spawn(async move |view, cx| {
            let result = runtime
                .request(move |services| async move {
                    services
                        .messenger
                        .own_profile(&services.sync, &session.owner, session.generation)
                        .await
                })
                .await;
            let _ = view.update(cx, |view, cx| {
                if let Ok(Ok(profile)) = result {
                    view.away |= profile.presence == Some(ProfilePresence::DoNotDisturb);
                    view.text.update(cx, |input, cx| {
                        input.set_text(profile.status_text.as_deref().unwrap_or_default(), cx);
                        input.set_enabled(true, cx);
                    });
                    view.emoji = profile.status_emoji.unwrap_or_default();
                    view.loaded = InitialStatus::Ready;
                } else {
                    view.loaded = InitialStatus::Failed;
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Разрешает менять поля только после загрузки и до запуска записи.
    fn can_edit(&self) -> bool {
        self.loaded == InitialStatus::Ready && self.can_dismiss()
    }

    /// Уточняет, можно ли отменить модалку до окончания текущего запроса.
    pub(super) fn can_dismiss(&self) -> bool {
        self.save.is_none()
    }

    /// Возвращает указатель и клавиатуру к строке статуса.
    fn cancel(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.can_dismiss() {
            let _ = self
                .host
                .update(cx, |host, cx| host.close_modal(window, cx));
        }
    }

    /// Обнуляет поля формы. Сервер обновится только по кнопке сохранения.
    fn clear(&mut self, cx: &mut Context<Self>) {
        if !self.can_edit() {
            return;
        }
        self.emoji.clear();
        self.away = false;
        self.picker = false;
        self.text.update(cx, TextInput::clear);
        cx.notify();
    }

    /// Сохраняет статус и оставляет форму открытой, если сервер отклонил запрос.
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_edit() {
            return;
        }
        let edit = StatusEdit {
            text: self.text.read(cx).text().to_owned(),
            emoji: self.emoji.clone(),
            away: self.away,
        };
        let away = self.away;
        let runtime = self.runtime.clone();
        let session = self.session.clone();
        self.failed = false;
        self.text
            .update(cx, |input, cx| input.set_enabled(false, cx));
        self.save = Some(cx.spawn_in(window, async move |view, cx| {
            let result = runtime
                .request(move |services| async move {
                    services
                        .messenger
                        .edit_own_status(&services.sync, &session.owner, session.generation, edit)
                        .await
                })
                .await;
            let _ = cx.update(|window, app| {
                let _ = view.update(app, |view, cx| {
                    view.save = None;
                    if let Ok(Ok(profile)) = result {
                        let _ = view
                            .root
                            .update(cx, |root, cx| root.status_saved(profile, away, cx));
                        view.cancel(window, cx);
                    } else {
                        view.failed = true;
                        view.text
                            .update(cx, |input, cx| input.set_enabled(true, cx));
                    }
                    cx.notify();
                });
            });
        }));
        cx.notify();
    }

    /// Выбирает эмодзи из пресета или из сетки.
    fn select(&mut self, emoji: &str, cx: &mut Context<Self>) {
        if !self.can_edit() {
            return;
        }
        self.emoji = emoji.to_owned();
        self.picker = false;
        cx.notify();
    }

    /// Открывает набор эмодзи и передаёт фокус его строке поиска.
    fn toggle_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_edit() {
            return;
        }
        self.picker = !self.picker;
        if self.picker {
            self.search.update(cx, TextInput::clear);
            self.search.read(cx).focus(window);
        }
        cx.notify();
    }

    /// Передаёт действие кнопки живой форме.
    fn handler(
        cx: &Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl Fn(&mut Window, &mut App) + 'static {
        let weak = cx.weak_entity();
        move |window, app| {
            let _ = weak.update(app, |view, cx| action(view, window, cx));
        }
    }

    /// Рисует один из шести быстрых вариантов с отметкой текущего выбора.
    fn preset(
        &self,
        index: usize,
        emoji: &'static str,
        palette: Palette,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let button = div()
            .id(("profile-status-preset", index))
            .size(px(36.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .border_1()
            .border_color(if self.emoji == emoji {
                palette.accent
            } else {
                palette.border
            })
            .bg(if self.emoji == emoji {
                palette.accent.opacity(0.15)
            } else {
                palette.background
            })
            .hover(|style| style.bg(palette.card_hover))
            .text_size(px(17.))
            .child(emoji);
        activate(
            button,
            (index == 0).then_some(&self.initial),
            palette,
            Self::handler(cx, move |view, _, cx| view.select(emoji, cx)),
        )
    }

    /// Рисует поиск, категории и прокручиваемые результаты в открытом наборе.
    fn picker(&self, palette: Palette, cx: &Context<Self>) -> Div {
        let language = Preferences::read(cx).language;
        let query = self.search.read(cx).text().trim().to_lowercase();
        let (category, title) = CATEGORIES
            .get(self.category)
            .copied()
            .unwrap_or(("suggested", "desktop.status.emoji.recent"));
        let found = EMOJIS.lines().filter_map(|line| {
            let (group, value) = line.split_once('\t')?;
            let (emoji, name) = value.split_once('\t')?;
            let matches = if query.is_empty() {
                group == category
            } else {
                name.to_lowercase().contains(&query) || emoji.contains(&query)
            };
            matches.then_some(emoji)
        });
        let recent = [self.emoji.clone()]
            .into_iter()
            .filter(|emoji| category == "suggested" && query.is_empty() && !emoji.is_empty());
        let items = recent.chain(found.map(str::to_owned));
        div()
            .h(px(320.))
            .flex()
            .flex_col()
            .rounded(px(8.))
            .border_1()
            .border_color(palette.border)
            .bg(palette.surface)
            .overflow_hidden()
            .child(div().px_3().pt_4().pb_3().child(self.search.clone()))
            .child(div().flex().justify_between().px_3().pb_3().children(
                std::iter::once(8).chain(0..CATEGORIES.len()).map(|index| {
                    let selected = index == self.category;
                    let icon = category_icon(
                        index,
                        if selected {
                            palette.accent
                        } else {
                            palette.text_muted
                        },
                    );
                    activate(
                        div()
                            .id(("profile-status-emoji-category", index))
                            .size(px(32.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .hover(|style| style.bg(palette.card_hover))
                            .child(icon),
                        None,
                        palette,
                        Self::handler(cx, move |view, _, cx| {
                            view.category = index;
                            view.search.update(cx, TextInput::clear);
                            cx.notify();
                        }),
                    )
                }),
            ))
            .child(
                div()
                    .id("profile-status-emoji-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_3()
                    .child(
                        label(
                            palette,
                            TextRole::Heading,
                            language.tr(if query.is_empty() {
                                title
                            } else {
                                "desktop.status.emoji.results"
                            }),
                        )
                        .text_color(palette.text_muted)
                        .pb_1(),
                    )
                    .child(div().flex().flex_wrap().children(items.enumerate().map(
                        |(index, emoji)| {
                            activate(
                                div()
                                    .id(("profile-status-emoji", index))
                                    .size(px(40.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .text_size(px(24.))
                                    .child(emoji.clone())
                                    .hover(|style| style.bg(palette.card_hover)),
                                None,
                                palette,
                                Self::handler(cx, move |view, _, cx| view.select(&emoji, cx)),
                            )
                        },
                    ))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(palette.border)
                    .child(
                        label(palette, TextRole::Heading, "😊")
                            .text_size(px(35.))
                            .line_height(px(48.)),
                    )
                    .child(
                        label(
                            palette,
                            TextRole::Body,
                            language.tr("desktop.status.emoji.mood"),
                        )
                        .text_color(palette.text_muted),
                    ),
            )
    }
}

/// Показывает одну ячейку оригинального спрайта категорий размером 28 px.
fn category_icon(index: usize, color: gpui::Hsla) -> Div {
    let side = px(28.);
    div().relative().size(side).overflow_hidden().child(
        gpui::svg()
            .path("icons/status-emoji-categories.svg")
            .absolute()
            .w(side * 10.)
            .h(side * 4.)
            .top_0()
            .left(side * -(index as f32))
            .text_color(color),
    )
}

impl Render for Status {
    /// Показывает заголовок, поля и действия как отдельную поверхность над окном.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.loaded == InitialStatus::Ready && std::mem::take(&mut self.first_frame) {
            window.focus(&self.initial);
        }
        let palette = Palette::for_window(window, cx);
        let language = Preferences::read(cx).language;
        self.text.update(cx, |input, cx| {
            input.set_placeholder(language.tr("settings.statusPlaceholder"), cx);
        });
        self.search.update(cx, |input, cx| {
            input.set_placeholder(language.tr("desktop.status.emojiSearch"), cx);
        });
        let busy = !self.can_edit();
        let ready = self.loaded == InitialStatus::Ready;
        let mut picker = Button::new(
            "profile-status-emoji-picker",
            language.tr("settings.statusChooseEmoji"),
        );
        picker.disabled = busy;
        let mut clear = Button::new("profile-status-clear", language.tr("settings.statusClear"));
        clear.appearance = ButtonAppearance::Ghost;
        clear.disabled = busy;
        let mut cancel = Button::new("profile-status-cancel", language.tr("common.cancel"));
        cancel.appearance = ButtonAppearance::Ghost;
        cancel.disabled = !self.can_dismiss();
        let mut save = Button::new("profile-status-save", language.tr("common.save"));
        save.appearance = ButtonAppearance::Primary;
        save.disabled = busy;
        let selected = if self.emoji.is_empty() {
            SharedString::from("🙂")
        } else {
            self.emoji.clone().into()
        };
        let checkbox = div()
            .id("profile-status-away")
            .debug_selector(|| "profile-status-away".into())
            .flex()
            .items_center()
            .gap_2()
            .child(Checkbox::square(
                "status-away-checkbox",
                self.away,
                palette,
                Motion::from_preference(ready && Preferences::animations_enabled(cx)),
            ))
            .child(label(
                palette,
                TextRole::Body,
                language.tr("settings.statusAwayToggle"),
            ));
        let checkbox = if busy {
            checkbox
        } else {
            activate(
                checkbox,
                None,
                palette,
                Self::handler(cx, |view, _, cx| {
                    view.away = !view.away;
                    cx.notify();
                }),
            )
        };
        div()
            .id("profile-status-dialog")
            .debug_selector(|| "profile-status-dialog".into())
            .w(px(448.).min((window.viewport_size().width - px(24.)).max(px(0.))))
            .max_h((window.viewport_size().height - px(24.)).max(px(0.)))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .rounded(px(10.))
            .border_1()
            .border_color(palette.border)
            .bg(palette.surface)
            .shadow_md()
            .child(
                div()
                    .flex_none()
                    .px_5()
                    .py_4()
                    .border_b_1()
                    .border_color(palette.border)
                    .child(label(
                        palette,
                        TextRole::Heading,
                        language.tr("settings.status"),
                    ))
                    .child(
                        label(
                            palette,
                            TextRole::Body,
                            language.tr("settings.statusDialogHint"),
                        )
                        .mt_1()
                        .text_color(palette.text_muted),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .px_5()
                    .py_4()
                    .children(match self.loaded {
                        InitialStatus::Ready => None,
                        InitialStatus::Loading => Some(
                            label(
                                palette,
                                TextRole::Body,
                                language.tr("desktop.status.loading"),
                            )
                            .into_any_element(),
                        ),
                        InitialStatus::Failed => Some(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(
                                    label(
                                        palette,
                                        TextRole::Body,
                                        language.tr("desktop.status.loadFailed"),
                                    )
                                    .text_color(palette.danger),
                                )
                                .child(
                                    Button::new(
                                        "profile-status-retry",
                                        language.tr("common.retry"),
                                    )
                                    .render(
                                        palette,
                                        None,
                                        Self::handler(cx, |view, _, cx| view.load_initial(cx)),
                                    ),
                                )
                                .into_any_element(),
                        ),
                    })
                    .child(
                        div()
                            .id("profile-status-fields")
                            .debug_selector(move || {
                                if ready {
                                    "profile-status-ready".into()
                                } else {
                                    "profile-status-loading".into()
                                }
                            })
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .children(
                                PRESETS
                                    .into_iter()
                                    .enumerate()
                                    .map(|(index, emoji)| self.preset(index, emoji, palette, cx)),
                            )
                            .child(
                                picker
                                    .render(
                                        palette,
                                        None,
                                        Self::handler(cx, |view, window, cx| {
                                            view.toggle_picker(window, cx)
                                        }),
                                    )
                                    .debug_selector(|| "profile-status-emoji-picker".into())
                                    .h(px(36.))
                                    .px(px(10.))
                                    .bg(palette.background)
                                    .border_color(if self.picker {
                                        palette.accent
                                    } else {
                                        palette.border
                                    }),
                            ),
                    )
                    .when(self.picker, |body| body.child(self.picker(palette, cx)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .child(
                                label(palette, TextRole::Body, language.tr("settings.status"))
                                    .text_color(palette.text_muted),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .h(px(42.))
                                    .px_3()
                                    .rounded(px(6.))
                                    .border_1()
                                    .border_color(palette.border)
                                    .bg(palette.background)
                                    .child(label(palette, TextRole::Heading, selected))
                                    .child(div().flex_1().min_w_0().child(self.text.clone())),
                            ),
                    )
                    .child(checkbox)
                    .when(self.failed, |body| {
                        body.child(
                            label(
                                palette,
                                TextRole::Body,
                                language.tr("settings.statusUpdateError"),
                            )
                            .text_color(palette.danger),
                        )
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px_5()
                    .py_3()
                    .border_t_1()
                    .border_color(palette.border)
                    .child(
                        clear
                            .render(
                                palette,
                                None,
                                Self::handler(cx, |view, _, cx| view.clear(cx)),
                            )
                            .h(px(36.))
                            .px_3()
                            .text_color(palette.text_muted),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                cancel
                                    .render(
                                        palette,
                                        None,
                                        Self::handler(cx, |view, window, cx| {
                                            view.cancel(window, cx)
                                        }),
                                    )
                                    .h(px(36.))
                                    .px_3()
                                    .text_color(palette.text_muted),
                            )
                            .child(
                                save.render(
                                    palette,
                                    None,
                                    Self::handler(cx, |view, window, cx| view.save(window, cx)),
                                )
                                .debug_selector(|| "profile-status-save".into())
                                .h(px(36.))
                                .px_4(),
                            ),
                    ),
            )
    }
}
