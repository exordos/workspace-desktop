//! Показывает общие настройки и раскрывает их варианты прямо в сайдбаре.
//! Выбор сохраняется в профиле. Горячие клавиши открываются поверх окна.

use std::collections::HashMap;

use gpui::{
    AnyElement, Context, Div, FocusHandle, ScrollHandle, SharedString, Stateful, WeakEntity,
    Window, div, prelude::*, px,
};

use crate::gpui_client::{
    contracts::AccountSession,
    notification_audio,
    preferences::{
        ChatClick, IdleTimeout, Language, NotificationSound, PreferenceValues, Preferences,
    },
    right_panel::RightPanel,
    runtime::RuntimeClient,
    ui::{Icon, Palette, activate, switch_indicator},
};

#[path = "hotkeys.rs"]
mod hotkeys;
pub(in crate::gpui_client::client) use hotkeys::Hotkeys;

/// Разделы, которые пользователь может оставить открытыми одновременно.
#[derive(Clone, Copy)]
enum Section {
    ChatClick, // Маршрут строки канала.
    Sound,     // Звук уведомлений.
    Language,  // Язык приложения.
    Timeout,   // Автоматический выход.
}

/// Страница общих настроек для активного входа.
pub(super) struct General {
    host: WeakEntity<RightPanel>, // Стек, который показывает окно горячих клавиш.
    session: AccountSession,      // Владелец страницы и её модалки.
    expanded: [bool; 4],          // Видимость каждого списка вариантов.
    focus: HashMap<SharedString, FocusHandle>, // Устойчивый фокус кнопок этого показа.
    scroll: ScrollHandle,         // Положение страницы, пока открыта модалка.
    storage_error: bool,          // Последнее изменение не удалось сохранить.
}

/// Текст и правая сторона строки рабочей панели.
struct Row {
    id: &'static str,               // Идентификатор действия и его фокуса.
    label: &'static str,            // Основная подпись.
    subtitle: Option<&'static str>, // Вторая строка или None.
    icon: Icon,                     // Иконка в квадрате 32 px.
    tail: AnyElement,               // Стрелка, выбранное значение или переключатель.
}

/// Вариант раскрытого списка, связанный с сохранением значения.
struct OptionRow<Choose = fn(&mut PreferenceValues)> {
    id: SharedString,               // Устойчивый ID строки, в том числе из кода языка.
    label: &'static str,            // Имя значения.
    subtitle: Option<&'static str>, // Подсказка при наличии.
    active: bool,                   // Показывает галочку выбора.
    choose: Choose,                 // Записывает значение в снимок профиля.
    preview: Option<NotificationSound>, // Звук для предпрослушивания у варианта выбора.
}

impl General {
    /// Создаёт свёрнутую страницу настроек для выбранного аккаунта.
    pub(super) fn new(
        host: WeakEntity<RightPanel>,
        session: AccountSession,
        _runtime: RuntimeClient,
        _: &mut Context<Self>,
    ) -> Self {
        Self {
            host,
            session,
            expanded: [false; 4],
            focus: HashMap::new(),
            scroll: ScrollHandle::new(),
            storage_error: false,
        }
    }

    /// Возвращает фокус одного элемента при повторной отрисовке.
    fn focus(&mut self, id: impl Into<SharedString>, cx: &mut Context<Self>) -> FocusHandle {
        self.focus
            .entry(id.into())
            .or_insert_with(|| cx.focus_handle().tab_stop(true))
            .clone()
    }

    /// Рисует строку референса с отступом 8 px и общим активным фокусом.
    fn row(
        &mut self,
        row: Row,
        palette: Palette,
        cx: &mut Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        let handle = self.focus(row.id, cx);
        let id = row.id;
        let weak = cx.weak_entity();
        activate(
            div()
                .id(id)
                .debug_selector(move || id.into())
                .w_full()
                .flex()
                .justify_between()
                .items_center()
                .gap(px(8.))
                .px(px(8.))
                .py(px(6.))
                .hover(|style| style.bg(palette.card_hover))
                .child(
                    div()
                        .flex()
                        .min_w_0()
                        .min_h(px(32.))
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .size(px(32.))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(row.icon.element(px(22.), palette.text_secondary)),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .text_size(px(14.))
                                .line_height(px(20.))
                                .text_color(palette.text)
                                .child(row.label)
                                .children(row.subtitle.map(|subtitle| {
                                    div()
                                        .mt(px(4.))
                                        .text_size(px(12.))
                                        .line_height(px(20.))
                                        .text_color(palette.text_muted)
                                        .child(subtitle)
                                })),
                        ),
                )
                .child(row.tail),
            Some(&handle),
            palette,
            move |window, app| {
                let _ = weak.update(app, |view, cx| action(view, window, cx));
            },
        )
    }

    /// Рисует раскрываемую строку со стрелкой и её текущим значением.
    fn section(
        &mut self,
        section: Section,
        id: &'static str,
        text: (&'static str, Option<&'static str>, Option<&'static str>),
        icon: Icon,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let (label, subtitle, value) = text;
        let arrow = if self.expanded[section as usize] {
            Icon::ChevronUp
        } else {
            Icon::ChevronRight
        };
        let color = value.map_or(palette.text, |_| palette.text_secondary);
        let tail = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .min_w_0()
            .text_size(px(14.))
            .line_height(px(20.))
            .text_color(color)
            .children(value.map(|value| div().min_w_0().child(value)))
            .child(arrow.element(px(16.), color))
            .into_any_element();
        self.row(
            Row {
                id,
                label,
                subtitle,
                icon,
                tail,
            },
            palette,
            cx,
            move |view, _, cx| {
                let item = &mut view.expanded[section as usize];
                *item = !*item;
                cx.notify();
            },
        )
    }

    /// Показывает открытый список вариантов с разделителями и галочкой.
    fn options<Choose: Fn(&mut PreferenceValues) + 'static>(
        &mut self,
        section: Section,
        rows: impl IntoIterator<Item = OptionRow<Choose>>,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        if !self.expanded[section as usize] {
            return None;
        }
        let mut list = div()
            .mx(px(8.))
            .rounded(px(8.))
            .border_1()
            .border_color(palette.border)
            .bg(palette.surface.opacity(0.6))
            .overflow_hidden();
        let weak = cx.weak_entity();
        for (index, row) in rows.into_iter().enumerate() {
            if index != 0 {
                list = list.child(div().mx(px(8.)).h(px(1.)).bg(palette.border));
            }
            let focus = self.focus(row.id.clone(), cx);
            let weak = weak.clone();
            let id = row.id;
            let choose = row.choose;
            let preview = row.preview;
            list = list.child(activate(
                div()
                    .id(id.clone())
                    .debug_selector(move || id.to_string())
                    .flex()
                    .justify_between()
                    .items_start()
                    .gap(px(12.))
                    .p(px(8.))
                    .hover(|style| style.bg(palette.card_hover))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(14.))
                            .line_height(px(16.))
                            .when(row.active, |label| {
                                label.font_weight(gpui::FontWeight::MEDIUM)
                            })
                            .child(row.label)
                            .children(row.subtitle.map(|subtitle| {
                                div()
                                    .mt(px(4.))
                                    .text_size(px(12.))
                                    .line_height(px(20.))
                                    .text_color(palette.text_secondary)
                                    .child(subtitle)
                            })),
                    )
                    .when(row.active, |element| {
                        element.child(Icon::Check.element(px(16.), palette.accent))
                    }),
                Some(&focus),
                palette,
                move |window, app| {
                    let _ = weak.update(app, |view, cx| {
                        if view.save(window, cx, &choose)
                            && let Some(sound) = preview
                        {
                            notification_audio::preview(sound);
                        }
                    });
                },
            ));
        }
        Some(list)
    }

    /// Сохраняет выбор и обновляет заинтересованные области окна.
    fn save(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        choose: impl FnOnce(&mut PreferenceValues),
    ) -> bool {
        if !Preferences::is_selected(cx, &self.session.owner) {
            return false;
        }
        self.storage_error = Preferences::update(cx, choose).is_err();
        cx.notify();
        window.refresh();
        !self.storage_error
    }

    /// Рисует переключатель 36 × 20 px для локального параметра.
    fn switch(
        &mut self,
        (id, label, icon): (&'static str, &'static str, Icon),
        checked: bool,
        palette: Palette,
        cx: &mut Context<Self>,
        toggle: fn(&mut PreferenceValues),
    ) -> Stateful<Div> {
        self.row(
            Row {
                id,
                label,
                icon,
                subtitle: None,
                tail: switch_indicator(palette, checked).into_any_element(),
            },
            palette,
            cx,
            move |view, window, cx| {
                view.save(window, cx, toggle);
            },
        )
    }

    /// Открывает диалог над всем окном и возвращает фокус в эту строку при закрытии.
    fn open_hotkeys(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let opener = self.focus("profile-hotkeys", cx);
        Self::open_dialog(self.host.clone(), &self.session, opener, window, cx);
    }

    /// Использует тот же диалог из кнопки страницы или из сочетания помощи.
    pub(super) fn open_dialog(
        host: WeakEntity<RightPanel>,
        session: &AccountSession,
        opener: FocusHandle,
        window: &mut Window,
        app: &mut gpui::App,
    ) {
        let modal_host = host.clone();
        let _ = host.update(app, |panel, cx| {
            panel.open_modal(session, opener, window, cx, |_, cx| {
                cx.new(|cx| hotkeys::Hotkeys::new(modal_host, session.owner.clone(), cx))
            });
        });
    }
}

impl Render for General {
    /// Показывает настройки в порядке Electron и раскрывает только выбранные группы.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_window(window, cx);
        let settings = Preferences::read(cx);
        let (lang, chat_click, sound, dock_bounce, separate_call_window, idle_timeout) = (
            settings.language,
            settings.chat_click,
            settings.notification_sound,
            settings.dock_bounce,
            settings.separate_call_window,
            settings.idle_timeout,
        );
        let shortcuts = self.row(
            Row {
                id: "profile-hotkeys",
                label: lang.tr("hotkeys.title"),
                subtitle: None,
                icon: Icon::Command,
                tail: Icon::ChevronRight
                    .element(px(16.), palette.text)
                    .into_any_element(),
            },
            palette,
            cx,
            Self::open_hotkeys,
        );
        div()
            .id("profile-settings-page")
            .debug_selector(|| "profile-settings-page".into())
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .pt(px(12.))
            .pb(px(20.))
            .text_color(palette.text)
            .child(shortcuts)
            .child(self.section(
                Section::ChatClick,
                "profile-chat-click",
                (
                    lang.tr("settings.chatClickAction"),
                    Some(lang.tr("settings.chatClickActionHint")),
                    None,
                ),
                Icon::ChatBubble,
                palette,
                cx,
            ))
            .children(self.options(
                Section::ChatClick,
                [
                    OptionRow::<fn(&mut PreferenceValues)> {
                        id: "profile-chat-click-general".into(),
                        label: lang.tr("settings.chatClickActionGeneral"),
                        subtitle: None,
                        active: chat_click == ChatClick::General,
                        choose: |value| value.chat_click = ChatClick::General,
                        preview: None,
                    },
                    OptionRow {
                        id: "profile-chat-click-latest".into(),
                        label: lang.tr("settings.chatClickActionLatestTopic"),
                        subtitle: Some(lang.tr("settings.chatClickActionAvatarHint")),
                        active: chat_click == ChatClick::LatestTopic,
                        choose: |value| value.chat_click = ChatClick::LatestTopic,
                        preview: None,
                    },
                ],
                palette,
                cx,
            ))
            .child(self.section(
                Section::Sound,
                "profile-sound",
                (
                    lang.tr("settings.notificationSound"),
                    None,
                    Some(sound_name(lang, sound)),
                ),
                Icon::VolumeUp,
                palette,
                cx,
            ))
            .children(self.options(Section::Sound, sound_options(lang, sound), palette, cx))
            .child(self.switch(
                (
                    "profile-dock-bounce",
                    lang.tr("settings.dockBounce"),
                    Icon::DesktopWindows,
                ),
                dock_bounce,
                palette,
                cx,
                |value| value.dock_bounce = !value.dock_bounce,
            ))
            .child(
                self.switch(
                    (
                        "profile-call-window",
                        lang.tr("desktop.general.separateCallWindow"),
                        Icon::Phone,
                    ),
                    separate_call_window,
                    palette,
                    cx,
                    |value| value.separate_call_window = !value.separate_call_window,
                )
                .gap(px(4.)),
            )
            .child(self.section(
                Section::Language,
                "profile-language",
                (lang.tr("settings.language"), None, Some(lang.name())),
                Icon::Language,
                palette,
                cx,
            ))
            .children(self.options(
                Section::Language,
                Language::available().map(|language| OptionRow {
                    id: format!("profile-language-{}", language.code()).into(),
                    label: language.name(),
                    subtitle: None,
                    active: lang == language,
                    choose: move |value: &mut PreferenceValues| value.language = language,
                    preview: None,
                }),
                palette,
                cx,
            ))
            .child(self.section(
                Section::Timeout,
                "profile-timeout",
                (
                    lang.tr("settings.authIdleTimeout"),
                    Some(lang.tr("settings.authIdleTimeoutHint")),
                    Some(timeout_name(lang, idle_timeout)),
                ),
                Icon::DeleteHistory,
                palette,
                cx,
            ))
            .children(self.options(
                Section::Timeout,
                timeout_options(lang, idle_timeout),
                palette,
                cx,
            ))
            .when(self.storage_error, |body| {
                body.child(
                    div()
                        .p(px(8.))
                        .text_color(palette.danger)
                        .child(lang.tr("desktop.general.saveFailed")),
                )
            })
    }
}

/// Возвращает подпись звука в строке и списке выбора.
fn sound_name(lang: Language, sound: NotificationSound) -> &'static str {
    match sound {
        NotificationSound::Default => lang.tr("settings.soundDefault"),
        NotificationSound::Subtle => lang.tr("settings.soundSubtle"),
        NotificationSound::Digital => lang.tr("settings.soundDigital"),
        NotificationSound::Glass => lang.tr("settings.soundGlass"),
        NotificationSound::Pulse => lang.tr("settings.soundPulse"),
        NotificationSound::None => lang.tr("settings.soundNone"),
    }
}

/// Собирает список звуков в порядке Electron.
fn sound_options(lang: Language, active: NotificationSound) -> impl Iterator<Item = OptionRow> {
    use NotificationSound::*;
    let definitions: [(_, _, fn(&mut PreferenceValues)); 6] = [
        ("profile-sound-default", Default, |value| {
            value.notification_sound = Default
        }),
        ("profile-sound-subtle", Subtle, |value| {
            value.notification_sound = Subtle
        }),
        ("profile-sound-digital", Digital, |value| {
            value.notification_sound = Digital
        }),
        ("profile-sound-glass", Glass, |value| {
            value.notification_sound = Glass
        }),
        ("profile-sound-pulse", Pulse, |value| {
            value.notification_sound = Pulse
        }),
        ("profile-sound-off", None, |value| {
            value.notification_sound = None
        }),
    ];
    definitions
        .into_iter()
        .map(move |(id, sound, choose)| OptionRow {
            id: id.into(),
            label: sound_name(lang, sound),
            subtitle: std::option::Option::None,
            active: active == sound,
            choose,
            preview: Some(sound),
        })
}

/// Возвращает подпись длительности на выбранном языке.
fn timeout_name(lang: Language, timeout: IdleTimeout) -> &'static str {
    match timeout {
        IdleTimeout::SixHours => lang.tr("settings.authIdleTimeout6h"),
        IdleTimeout::TwelveHours => lang.tr("settings.authIdleTimeout12h"),
        IdleTimeout::Day => lang.tr("settings.authIdleTimeout24h"),
        IdleTimeout::ThreeDays => lang.tr("settings.authIdleTimeout3d"),
        IdleTimeout::Week => lang.tr("settings.authIdleTimeout7d"),
        IdleTimeout::Never => lang.tr("settings.authIdleTimeoutNever"),
    }
}

/// Собирает список тайм-аутов в порядке длительности.
fn timeout_options(lang: Language, active: IdleTimeout) -> impl Iterator<Item = OptionRow> {
    use IdleTimeout::*;
    let definitions: [(_, _, fn(&mut PreferenceValues)); 6] = [
        ("profile-timeout-6h", SixHours, |value| {
            value.idle_timeout = SixHours
        }),
        ("profile-timeout-12h", TwelveHours, |value| {
            value.idle_timeout = TwelveHours
        }),
        ("profile-timeout-24h", Day, |value| value.idle_timeout = Day),
        ("profile-timeout-3d", ThreeDays, |value| {
            value.idle_timeout = ThreeDays
        }),
        ("profile-timeout-7d", Week, |value| {
            value.idle_timeout = Week
        }),
        ("profile-timeout-never", Never, |value| {
            value.idle_timeout = Never
        }),
    ];
    definitions
        .into_iter()
        .map(move |(id, timeout, choose)| OptionRow {
            id: id.into(),
            label: timeout_name(lang, timeout),
            subtitle: None,
            active: active == timeout,
            choose,
            preview: None,
        })
}
