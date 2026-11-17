//! Проверяет активацию примитивов и расположение текста в узком окне GPUI.

use gpui::{
    Context, FocusHandle, Modifiers, Render, TestAppContext, Window, div, point, prelude::*, px,
    size,
};

use super::*;

gpui::actions!(ui_tests, [NextFocus]);

/// Панель тестирует маршруты событий, которые получает экран.
struct Controls {
    container: FocusHandle, // Начальный фокус панели перед клавишей Tab.
    active: FocusHandle,    // Постоянный фокус доступной кнопки.
    blocked: FocusHandle,   // Недоступный элемент не должен попасть в обход Tab.
    presses: usize,         // Сколько действий получил экран.
    blocked_presses: usize, // Вызовы, которые ошибочно прошли через недоступную кнопку.
    hovered: bool,          // Указатель находится над доступной кнопкой.
}

impl Render for Controls {
    /// Показывает активную и недоступную кнопку в порядке обхода Tab.
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_mode(ThemeMode::Dark);
        let active_entity = cx.entity().downgrade();
        let blocked_entity = active_entity.clone();
        let mut blocked = Button::new("blocked", "Недоступная");
        blocked.disabled = true;
        div()
            .track_focus(&self.container)
            .size_full()
            .flex()
            .flex_col()
            .items_start()
            .on_action(|_: &NextFocus, window, _| window.focus_next())
            .child(
                blocked
                    .render(palette, Some(&self.blocked), move |_, app| {
                        let _ = blocked_entity.update(app, |view, _| view.blocked_presses += 1);
                    })
                    .debug_selector(|| "blocked-button".into()),
            )
            .child(
                Button::new("active", "Доступная")
                    .render(palette, Some(&self.active), move |_, app| {
                        let _ = active_entity.update(app, |view, _| view.presses += 1);
                    })
                    .on_hover(cx.listener(|view, hovered: &bool, _, cx| {
                        view.hovered = *hovered;
                        cx.notify();
                    }))
                    .debug_selector(|| "active-button".into()),
            )
    }
}

/// Недоступные кнопки пропускает Tab, а рабочая получает Enter, пробел и клик.
#[gpui::test]
fn buttons_accept_mouse_and_keyboard_only_when_enabled(cx: &mut TestAppContext) {
    cx.update(|app| app.bind_keys([gpui::KeyBinding::new("tab", NextFocus, None)]));
    let (root, ui) = cx.add_window_view(|window, cx| {
        let container = cx.focus_handle();
        window.focus(&container);
        Controls {
            container,
            active: cx.focus_handle(),
            blocked: cx.focus_handle(),
            presses: 0,
            blocked_presses: 0,
            hovered: false,
        }
    });
    ui.run_until_parked();
    ui.simulate_keystrokes("tab");
    ui.update(|window, app| {
        assert!(
            root.read(app).active.is_focused(window),
            "Tab должен обойти недоступную кнопку"
        );
        assert!(!root.read(app).blocked.is_focused(window));
    });
    // GPUI превращает отпускание Enter и пробела в активацию кнопки.
    for key in ["enter", "space"] {
        ui.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse(key).expect("клавиша активации"),
        });
    }
    let blocked = ui
        .debug_bounds("blocked-button")
        .expect("видна недоступная кнопка");
    ui.simulate_click(blocked.center(), Modifiers::none());
    let active = ui
        .debug_bounds("active-button")
        .expect("видна доступная кнопка");
    ui.simulate_mouse_move(active.center(), None, Modifiers::none());
    ui.run_until_parked();
    ui.read(|app| assert!(root.read(app).hovered, "указатель должен войти в кнопку"));
    ui.simulate_click(active.center(), Modifiers::none());
    ui.simulate_mouse_move(point(px(250.), px(150.)), None, Modifiers::none());
    ui.run_until_parked();
    ui.read(|app| {
        let view = root.read(app);
        assert!(!view.hovered, "указатель должен покинуть кнопку");
        assert_eq!(
            view.presses, 3,
            "Enter, пробел и клик должны дать три действия"
        );
        assert_eq!(
            view.blocked_presses, 0,
            "недоступная кнопка не должна вызывать действие"
        );
    });
}

/// Один и тот же длинный разговор в обеих темах.
struct NarrowRow(ThemeMode); // Палитра тестового окна.

impl Render for NarrowRow {
    /// Рисует разговор, чьи название и превью не помещаются целиком.
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().w_full().child(
            ChatRow {
                id: "narrow".into(),
                avatar: Avatar::from_initials("ДЧ"),
                title: "Название очень длинного разговора с несколькими словами".into(),
                preview: Some(ChatRowPreview {
                    sender: Some("Собеседник".into()),
                    text: "Очень длинное превью\nсообщения".into(),
                }),
                time: Some("10:13".into()),
                unread: Some((123, BadgeTone::Accent)),
                mention: true,
                selected: true,
                disabled: false,
            }
            .render(Palette::for_mode(self.0), None, |_, _| {})
            .debug_selector(|| "narrow-chat-row".into()),
        )
    }
}

/// Название сжимается раньше счётчиков, поэтому они остаются в пределах окна.
#[gpui::test]
fn chat_title_does_not_displace_metadata_in_narrow_window(cx: &mut TestAppContext) {
    for mode in [ThemeMode::Light, ThemeMode::Dark] {
        let (_, ui) = cx.add_window_view(|_, _| NarrowRow(mode));
        ui.simulate_resize(size(px(184.), px(140.)));
        ui.run_until_parked();
        let row = ui.debug_bounds("narrow-chat-row").expect("строка чата");
        let title = ui.debug_bounds("chat-row-title").expect("название чата");
        let metadata = ui
            .debug_bounds("chat-row-metadata")
            .expect("время и счётчики");
        let avatar = ui.debug_bounds("avatar").expect("аватар строки");
        let unread = ui.debug_bounds("unread-badge").expect("бейдж счётчика");
        let mention = ui.debug_bounds("mention-badge").expect("бейдж упоминания");
        assert!(title.size.width > px(0.), "{mode:?}. {title:?}");
        assert!(
            title.right() < metadata.left(),
            "{mode:?}. {title:?} {metadata:?}"
        );
        assert!(
            metadata.right() <= row.right() && row.right() <= px(184.),
            "{mode:?}. {row:?} {metadata:?}"
        );
        assert_eq!(
            row.size.height,
            px(60.),
            "{mode:?}. длинный текст не должен увеличить высоту: {row:?}"
        );
        assert_eq!(avatar.size, size(px(40.), px(40.)));
        assert_eq!(unread.size.height, px(20.));
        assert!(unread.size.width >= px(20.));
        assert_eq!(mention.size, size(px(20.), px(20.)));
    }
}

/// Не даёт светлому тексту на синем счётчике или приглушённому фону снизить читаемость.
#[test]
fn unread_badges_keep_text_contrast_in_both_themes() {
    for mode in [ThemeMode::Light, ThemeMode::Dark] {
        let palette = Palette::for_mode(mode);
        for (foreground, background) in [
            (palette.badge_text, palette.unread),
            (palette.muted_badge_text, palette.unread_muted),
        ] {
            let a = relative_luminance(foreground);
            let b = relative_luminance(background);
            let contrast = (a.max(b) + 0.05) / (a.min(b) + 0.05);
            assert!(contrast >= 4.5, "{mode:?}: {contrast:.2}:1");
        }
    }
}

/// Переводит цвет sRGB в относительную яркость для проверки контраста.
fn relative_luminance(color: gpui::Hsla) -> f32 {
    let color = color.to_rgb();
    [color.r, color.g, color.b]
        .into_iter()
        .zip([0.2126, 0.7152, 0.0722])
        .map(|(channel, weight)| {
            let linear = if channel <= 0.04045 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            };
            linear * weight
        })
        .sum()
}
