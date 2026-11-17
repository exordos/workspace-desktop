//! Собирает две палитры из общих компонентов и показывает переходы фокуса, выбор и ввод.

use gpui::{
    App, Application, Bounds, Context, Div, Entity, Focusable, KeyBinding, SharedString,
    Subscription, TitlebarOptions, Window, WindowBounds, WindowOptions, div, prelude::*, px, size,
};
use workspace_core::context::{AccountId, OwnerContext, SessionGeneration};
use workspace_desktop::gpui_client::{
    composer::Editor,
    contracts::AccountSession,
    ui::{
        Avatar, AvatarPresence, AvatarSize, BadgeTone, Button, ButtonAppearance, ChatRow,
        ChatRowPreview, DeliveryMark, FieldAppearance, Icon, MessageBubble, Palette,
        SEARCH_ICON_SIZE, TextRole, ThemeMode, TopBar, TopBarAccount, TopBarSelection,
        WorkspaceAssets, field, label,
    },
};

// Tab использует тот же обход сфокусированных элементов, что и рабочее окно.
gpui::actions!(gallery, [NextControl, PreviousControl]);

/// Состояние демонстрационной панели в одной палитре.
struct Panel {
    mode: ThemeMode,       // Палитра, которую показывает панель.
    id: &'static str,      // Префикс ключей элементов панели.
    selected: u8,          // Номер выбранной строки каталога.
    draft: Entity<Editor>, // Общий редактор с локальным черновиком.
}

impl Panel {
    /// Создаёт редактор и выбирает первую строку примера.
    fn new(mode: ThemeMode, id: &'static str, cx: &mut Context<Self>) -> Self {
        Self {
            mode,
            id,
            selected: 0,
            draft: cx.new(Editor::for_conversation),
        }
    }

    /// Рисует панель с каталогом, сообщениями, полями и кнопками.
    fn body(&self, palette: Palette, cx: &mut Context<Self>) -> Div {
        let theme = match self.mode {
            ThemeMode::Light => "Светлая тема",
            ThemeMode::Dark => "Тёмная тема",
        };
        let mut ready = Button::new("ready", "Выбрать канал");
        ready.icon = Some(Icon::Chat);
        let mut disabled = Button::new("disabled", "Отправить");
        disabled.icon = Some(Icon::Send);
        disabled.appearance = ButtonAppearance::Primary;
        disabled.disabled = true;
        let target = cx.entity().downgrade();

        div()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .child(label(palette, TextRole::Heading, theme))
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_wrap()
                    .gap_2()
                    .child(avatar("АК", AvatarSize::Small, AvatarPresence::Online).render(palette))
                    .child(avatar("МП", AvatarSize::Medium, AvatarPresence::Away).render(palette))
                    .child(Avatar::from_initials("").render(palette))
                    .child(ready.render(palette, None, move |_, app| {
                        let _ = target.update(app, |panel, cx| {
                            panel.selected = 1;
                            cx.notify();
                        });
                    }))
                    .child(disabled.render(palette, None, |_, _| {})),
            )
            .child(field(
                palette,
                FieldAppearance::Normal,
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(Icon::Search.element(SEARCH_ICON_SIZE, palette.text_secondary))
                    .child(label(palette, TextRole::Body, "Поиск по чатам")),
            ))
            .child(self.chat_row(palette, 0, cx))
            .child(self.chat_row(palette, 1, cx))
            .child(self.chat_row(palette, 2, cx))
            .child(
                MessageBubble {
                    author: Some("Мария".into()),
                    body: "Собрала примеры для обсуждения. Длинное сообщение переносится по ширине окна, а время остаётся рядом, пока хватает места."
                        .into(),
                    time: Some("10:07".into()),
                    own: false,
                    delivery: None,
                }
                .render(palette),
            )
            .child(delivery(palette, "Посмотрел, спасибо!", DeliveryMark::Read))
            .child(delivery(palette, "Повторю после проверки связи", DeliveryMark::Failed))
            .child(field(
                palette,
                FieldAppearance::Invalid,
                label(palette, TextRole::Body, "Не удалось загрузить результаты"),
            ))
            .child(field(
                palette,
                FieldAppearance::Disabled,
                label(palette, TextRole::Body, "Редактирование недоступно")
                    .text_color(palette.text_muted),
            ))
    }

    /// Рисует строки каталога с длинным кириллическим текстом, счётчиком и выбором.
    fn chat_row(&self, palette: Palette, index: u8, cx: &Context<Self>) -> impl IntoElement {
        let (initials, title, preview, unread) = match index {
            0 => (
                "МП",
                "Мария Петрова с очень длинным отображаемым именем",
                "Обновила материалы к встрече и добавила к ним вопросы для обсуждения",
                Some((12, BadgeTone::Accent)),
            ),
            1 => (
                "ПК",
                "Проектная команда",
                "План на следующую неделю",
                Some((3, BadgeTone::Muted)),
            ),
            _ => ("", "Закрытый разговор", "Нет доступа к истории", None),
        };
        let target = cx.entity().downgrade();
        let panel_id = self.id;
        ChatRow {
            id: ("gallery-chat", index as usize).into(),
            avatar: Avatar::from_initials(initials),
            title: title.into(),
            preview: Some(ChatRowPreview {
                sender: (index == 1).then(|| SharedString::from("Алексей")),
                text: preview.into(),
            }),
            time: (index < 2).then(|| "10:07".into()),
            unread,
            mention: index == 0,
            selected: self.selected == index,
            disabled: index == 2,
        }
        .render(palette, None, move |_, app| {
            let _ = target.update(app, |panel, cx| {
                panel.selected = index;
                cx.notify();
            });
        })
        .debug_selector(move || format!("{panel_id}-chat-{index}"))
    }
}

impl Render for Panel {
    /// Оставляет редактор видимым при прокрутке примеров внутри панели.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_mode(self.mode);
        let appearance = if self.draft.focus_handle(cx).is_focused(window) {
            FieldAppearance::Focused
        } else {
            FieldAppearance::Normal
        };
        div()
            .id(self.id)
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(palette.background)
            .child(
                div()
                    .id("examples")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(self.body(palette, cx)),
            )
            .child(div().p_4().bg(palette.surface).child(field(
                palette,
                appearance,
                self.draft.clone(),
            )))
    }
}

/// Окно с независимыми черновиками и выбором в двух палитрах.
struct Gallery {
    top_bar: Entity<TopBar>, // Общая шапка с четырьмя демонстрационными входами.
    _account_selection: Subscription, // Перерисовывает выбранный вход в шапке.
    light: Entity<Panel>,    // Светлая панель.
    dark: Entity<Panel>,     // Тёмная панель.
}

impl Gallery {
    /// Открывает обе палитры и ставит фокус в первый черновик.
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let light = cx.new(|cx| Panel::new(ThemeMode::Light, "light", cx));
        let dark = cx.new(|cx| Panel::new(ThemeMode::Dark, "dark", cx));
        window.focus(&light.read(cx).draft.focus_handle(cx));
        let top_bar = cx.new(TopBar::new);
        top_bar.update(cx, |view, cx| view.set_accounts(demo_accounts("A"), cx));
        let selection = cx.subscribe(&top_bar, |_, bar, chosen: &TopBarSelection, cx| {
            bar.update(cx, |view, cx| {
                view.set_accounts(demo_accounts(&chosen.0.owner.account_id.0), cx);
            });
        });
        Self {
            top_bar,
            _account_selection: selection,
            light,
            dark,
        }
    }
}

impl Render for Gallery {
    /// Рисует обе палитры в равных колонках и подключает обход Tab.
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .on_action(|_: &NextControl, window, _| window.focus_next())
            .on_action(|_: &PreviousControl, window, _| window.focus_prev())
            .child(self.top_bar.clone())
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .gap(px(1.))
                    .child(self.light.clone())
                    .child(self.dark.clone()),
            )
    }
}

/// Даёт шапке обычный вход, длинную подпись и переполнение быстрых кнопок.
fn demo_accounts(selected: &str) -> Vec<TopBarAccount> {
    [
        ("A", "Аккаунт A", None),
        ("B", "Аккаунт B", Some(5)),
        (
            "C",
            "Демонстрационный аккаунт с длинной подписью",
            Some(1001),
        ),
        ("D", "Аккаунт D", Some(42)),
    ]
    .into_iter()
    .map(|(id, label, unread)| TopBarAccount {
        session: AccountSession {
            owner: OwnerContext {
                account_id: AccountId(id.into()),
                server: "https://workspace.test".into(),
                organization_id: "demo".into(),
                project_id: "demo".into(),
                user_id: id.into(),
            },
            generation: SessionGeneration(1),
        },
        label: label.into(),
        detail: "workspace.test · demo".into(),
        selected: id == selected,
        unread,
    })
    .collect()
}

/// Готовит аватар нужного размера с выбранным состоянием присутствия.
fn avatar(initials: &'static str, size: AvatarSize, presence: AvatarPresence) -> Avatar {
    Avatar {
        size,
        presence,
        ..Avatar::from_initials(initials)
    }
}

/// Показывает итог отправки рядом со временем сообщения.
fn delivery(palette: Palette, body: &'static str, status: DeliveryMark) -> impl IntoElement {
    MessageBubble {
        author: None,
        body: body.into(),
        time: Some("10:08".into()),
        own: true,
        delivery: Some(status),
    }
    .render(palette)
}

/// Запускает галерею с общими ресурсами и системным вводом рабочего клиента.
pub fn run() {
    Application::new()
        .with_assets(WorkspaceAssets)
        .run(|app: &mut App| {
            Editor::bind_keys(app);
            app.bind_keys([
                KeyBinding::new("tab", NextControl, None),
                KeyBinding::new("shift-tab", PreviousControl, None),
            ]);
            TopBar::bind_keys(app);
            app.on_window_closed(|app| {
                if app.windows().is_empty() {
                    app.quit();
                }
            })
            .detach();
            let bounds = Bounds::centered(None, size(px(960.), px(860.)), app);
            app.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("Workspace GPUI Gallery".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window, app| app.new(|cx| Gallery::new(window, cx)),
            )
            .expect("Не удалось открыть галерею GPUI");
            app.activate(true);
        });
}

#[cfg(all(test, feature = "gpui-test"))]
mod tests {
    use gpui::{Modifiers, TestAppContext};

    use super::*;

    /// Выбирает строку мышью и проверяет изоляцию палитр и недоступную строку.
    #[gpui::test]
    fn catalog_selection_belongs_to_each_panel(cx: &mut TestAppContext) {
        let (gallery, ui) = cx.add_window_view(Gallery::new);
        ui.simulate_resize(size(px(720.), px(680.)));
        ui.run_until_parked();
        let second = ui.debug_bounds("dark-chat-1").expect("строка тёмной темы");
        ui.simulate_click(second.center(), Modifiers::none());
        ui.run_until_parked();
        let disabled = ui
            .debug_bounds("dark-chat-2")
            .expect("недоступная строка тёмной темы");
        ui.simulate_click(disabled.center(), Modifiers::none());
        ui.run_until_parked();
        ui.read(|app| {
            let gallery = gallery.read(app);
            assert_eq!(gallery.dark.read(app).selected, 1);
            assert_eq!(gallery.light.read(app).selected, 0);
        });
    }
}
