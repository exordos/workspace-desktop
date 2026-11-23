//! Проверяет, что отдельные кнопки карточки канала вызывают свои действия.

use gpui::{Context, KeyUpEvent, Keystroke, Modifiers, Render, TestAppContext};

use super::*;
use crate::gpui_client::ui::ThemeMode;

/// Действие, которое принял экран от карточки.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    Open,    // Открытие основного маршрута канала.
    Expand,  // Переключение раскрытия тем.
    Preview, // Открытие темы последнего сообщения.
    General, // Переход в общий чат через аватар.
}

/// Показывает одну карточку с четырьмя независимыми действиями.
struct Fixture {
    open_focus: FocusHandle,    // Фокус основного маршрута.
    expand_focus: FocusHandle,  // Фокус кнопки раскрытия.
    preview_focus: FocusHandle, // Фокус области превью.
    avatar_focus: FocusHandle,  // Фокус перехода в общую тему.
    preview_id: Option<Uuid>,   // Какое сообщение показывается в тестовой карточке.
    calls: Vec<Action>,         // Последовательность принятых действий.
}

impl Fixture {
    /// Связывает действие строки со списком событий тестового окна.
    fn record(cx: &Context<Self>, action: Action) -> RowAction {
        let entity = cx.entity().downgrade();
        Box::new(move |_, app| {
            let _ = entity.update(app, |view, _| view.calls.push(action));
        })
    }
}

impl Render for Fixture {
    /// Показывает длинный заголовок и непрочитанное в слоте стрелки.
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_mode(ThemeMode::Light);
        channel_card(palette).max_w(px(264.)).child(
            ChannelRow {
                id: Uuid::from_u128(1),
                title: "Название тестового канала должно помещаться в карточку".into(),
                details: ChatRowDetails {
                    preview: self.preview_id.map(|_| ChatRowPreview {
                        sender: Some("Автор".into()),
                        text: "Последнее сообщение".into(),
                    }),
                    unread: Some((2, BadgeTone::Accent)),
                    ..ChatRowDetails::default()
                },
                selected: false,
                color: Some(0x4162c1),
                expanded: Some(false),
            }
            .render(
                palette,
                Some(&self.open_focus),
                Some(&self.expand_focus),
                ChannelActions {
                    open: Some(Self::record(cx, Action::Open)),
                    toggle: Self::record(cx, Action::Expand),
                    preview: self.preview_id.map(|id| PreviewAction {
                        id,
                        focus: self.preview_focus.clone(),
                        on_open: Self::record(cx, Action::Preview),
                    }),
                    avatar: Some((self.avatar_focus.clone(), Self::record(cx, Action::General))),
                },
            ),
        )
    }
}

/// Создаёт тестовую карточку с подготовленным превью.
fn fixture(_: &mut Window, cx: &mut Context<Fixture>) -> Fixture {
    Fixture {
        open_focus: cx.focus_handle(),
        expand_focus: cx.focus_handle(),
        preview_focus: cx.focus_handle(),
        avatar_focus: cx.focus_handle(),
        preview_id: Some(Uuid::from_u128(2)),
        calls: Vec::new(),
    }
}

/// Клик и Enter передают только действие той области, на которую попал пользователь.
#[gpui::test]
fn channel_child_actions_do_not_open_the_parent_route(cx: &mut TestAppContext) {
    let (root, ui) = cx.add_window_view(fixture);
    ui.run_until_parked();

    for selector in [
        "catalog-stream-avatar-00000000-0000-0000-0000-000000000001",
        "catalog-stream-preview-00000000-0000-0000-0000-000000000001",
        "catalog-stream-expand-00000000-0000-0000-0000-000000000001",
    ] {
        let point = ui.debug_bounds(selector).expect(selector).center();
        ui.simulate_mouse_move(point, None, Modifiers::none());
        ui.run_until_parked();
        ui.simulate_click(point, Modifiers::none());
        ui.run_until_parked();
    }
    ui.simulate_event(KeyUpEvent {
        keystroke: Keystroke::parse("enter").expect("клавиша Enter"),
    });
    let title = ui.debug_bounds("chat-row-title").expect("название канала");
    ui.simulate_click(title.center(), Modifiers::none());
    ui.run_until_parked();
    ui.read(|app| {
        assert_eq!(
            root.read(app).calls,
            [
                Action::General,
                Action::Preview,
                Action::Expand,
                Action::Expand,
                Action::Open,
            ],
        );
    });
}

/// Обновление сообщения между нажатием и отпусканием не открывает другой маршрут.
#[gpui::test]
fn stale_preview_click_cannot_open_parent_or_new_preview(cx: &mut TestAppContext) {
    let (root, ui) = cx.add_window_view(fixture);
    ui.run_until_parked();

    let selector = "catalog-stream-preview-00000000-0000-0000-0000-000000000001";
    for next in [Some(Uuid::from_u128(3)), None] {
        let point = ui.debug_bounds(selector).expect(selector).center();
        ui.simulate_mouse_move(point, None, Modifiers::none());
        ui.run_until_parked();
        ui.simulate_mouse_down(point, MouseButton::Left, Modifiers::none());
        ui.update(|_, app| {
            root.update(app, |view, cx| {
                view.preview_id = next;
                cx.notify();
            })
        });
        ui.run_until_parked();
        ui.simulate_mouse_up(point, MouseButton::Left, Modifiers::none());
        ui.run_until_parked();
    }
    ui.read(|app| assert!(root.read(app).calls.is_empty()));
}
