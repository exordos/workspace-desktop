//! Проверяет, что бабл показывает только переданные итоги и отдаёт доступные действия.

use std::time::Duration;

use gpui::{
    Context, FocusHandle, KeyUpEvent, Keystroke, Modifiers, Pixels, Render, TestAppContext,
    VisualTestContext, Window, size,
};

use super::*;
use crate::gpui_client::ui::{ThemeMode, track_focus_input};

gpui::actions!(poll_tests, [NextControl]);

/// Окно с одним вариантом, чтобы проверять его геометрию и действия.
#[derive(Default)]
struct Preview {
    initial_focus: Option<FocusHandle>, // Начало обхода клавишей Tab.
    closed: bool,                       // Опрос завершён.
    pending: bool,                      // Вопрос занят.
    read_only: bool,                    // Бабл открыт для чтения.
    anonymous: bool,                    // Имена участников скрыты.
    show_results: bool,                 // Нужно показать результат и аватары.
    motion_enabled: Option<bool>,       // None — статичный сценарий, иначе режим анимаций.
    result_percent: Option<u8>,         // Итог после голосования или None до первого клика.
    actions: Vec<PollAction>,           // Намерения, полученные от компонента.
}

impl Render for Preview {
    /// Рисует один вопрос с длинным ответом и общим обходом фокуса.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let target = cx.entity().downgrade();
        let palette = Palette::for_mode(ThemeMode::Light).with_window_input(window, cx);
        let poll = PollBubble {
            id: "poll-preview".into(),
            closed: self.closed,
            read_only: self.read_only,
            anonymous: self.anonymous,
            multiple_answers: false,
            status_date: None,
            error: None,
            participants: self.show_results.then_some(12),
            view_votes: self.show_results,
            questions: vec![PollQuestion {
                id: "question".into(),
                text: "Когда провести встречу?".into(),
                participants: None,
                pending: self.pending,
                error: None,
                correct_answer: None,
                reserve_correct_answer: false,
                add_option: None,
                options: vec![PollOption {
                    id: "option".into(),
                    text: "В среду после обсуждения плана и итогов предыдущей встречи".into(),
                    selected: self.result_percent == Some(80),
                    enabled: true,
                    correctness: None,
                    result: self.show_results.then_some(PollResult {
                        votes: 6,
                        percent: self.result_percent.unwrap_or(50),
                    }),
                    voters: ["А", "Б", "В", "Г", "Д"]
                        .into_iter()
                        .map(Avatar::from_initials)
                        .collect(),
                }],
            }],
        };
        let message = MessageBubble {
            author: None,
            body: "".into(),
            time: Some("12:09".into()),
            own: false,
            delivery: None,
        };

        let on_action = move |action, _: &mut Window, app: &mut App| {
            let _ = target.update(app, |view, cx| {
                if view.motion_enabled.is_some()
                    && let PollAction::Vote { selected, .. } = &action
                {
                    view.show_results = true;
                    view.result_percent = Some(if *selected { 80 } else { 20 });
                    cx.notify();
                }
                view.actions.push(action);
            });
        };
        let poll = if let Some(enabled) = self.motion_enabled {
            poll.render_with_motion(
                message,
                palette,
                Language::Ru,
                false,
                Motion::from_preference(enabled),
                on_action,
            )
        } else {
            poll.render(message, palette, Language::Ru, false, on_action)
        };

        div()
            .size_full()
            .when_some(self.initial_focus.as_ref(), |root, focus| {
                root.track_focus(focus)
            })
            .child(track_focus_input(cx))
            .on_action(|_: &NextControl, window, _| window.focus_next())
            .child(poll)
    }
}

/// Читает ширину нарисованной полоски, если результаты уже открылись.
fn bar_width(ui: &mut VisualTestContext) -> Pixels {
    ui.debug_bounds("poll-result-bar")
        .expect("полоска результата")
        .size
        .width
}

/// Рисует кадр после клика или изменения тестового времени.
fn draw_poll(ui: &mut VisualTestContext) {
    ui.update(|window, app| window.draw(app).clear());
}

/// Проверяет первый результат, разворот с видимой ширины и выключение без повтора.
#[gpui::test]
fn poll_result_bar_animates_from_visible_width_and_finishes_when_disabled(cx: &mut TestAppContext) {
    let clock = cx.executor();
    let (preview, ui) = cx.add_window_view(|_, _| Preview {
        anonymous: true,
        motion_enabled: Some(true),
        ..Preview::default()
    });
    ui.simulate_resize(size(px(500.), px(480.)));
    draw_poll(ui);

    let choice = ui.debug_bounds("poll-choice").unwrap();
    ui.simulate_click(choice.center(), Modifiers::none());
    draw_poll(ui);
    assert_eq!(bar_width(ui), px(0.));
    assert!(ui.debug_bounds("poll-answer-with-result").is_some());
    let shown_bubble = ui.debug_bounds("poll-bubble").unwrap();
    let shown_choice = ui.debug_bounds("poll-choice").unwrap();

    clock.advance_clock(Duration::from_millis(100));
    draw_poll(ui);
    let growing = bar_width(ui);
    let full_width = shown_bubble.size.width - px(24.);
    assert!(growing > full_width * 0.2 && growing < full_width * 0.8);
    assert_eq!(ui.debug_bounds("poll-choice"), Some(shown_choice));
    assert_eq!(ui.debug_bounds("poll-bubble"), Some(shown_bubble));

    ui.simulate_click(shown_choice.center(), Modifiers::none());
    draw_poll(ui);
    assert_eq!(bar_width(ui), growing);
    clock.advance_clock(Duration::from_millis(60));
    draw_poll(ui);
    let shrinking = bar_width(ui);
    assert!(shrinking > full_width * 0.2 && shrinking < growing);

    for enabled in [false, true] {
        ui.update(|_, app| {
            preview.update(app, |preview, cx| {
                preview.motion_enabled = Some(enabled);
                cx.notify();
            });
        });
        draw_poll(ui);
        assert!((bar_width(ui) - full_width * 0.2).abs() <= px(1.));
    }
}

/// Уже готовый снимок при открытии истории сразу занимает итоговую ширину.
#[gpui::test]
fn poll_ready_result_is_static_on_mount(cx: &mut TestAppContext) {
    let (_, ui) = cx.add_window_view(|_, _| Preview {
        anonymous: true,
        show_results: true,
        motion_enabled: Some(true),
        ..Preview::default()
    });
    ui.simulate_resize(size(px(500.), px(480.)));
    draw_poll(ui);
    let full_width = ui.debug_bounds("poll-bubble").unwrap().size.width - px(24.);
    assert!((bar_width(ui) - full_width * 0.5).abs() <= px(1.));
}

/// Мышь и клавиатура вызывают одно действие, пока вопрос доступен для ответа.
#[gpui::test]
fn vote_only_activates_when_available(cx: &mut TestAppContext) {
    cx.update(|app| app.bind_keys([gpui::KeyBinding::new("tab", NextControl, None)]));
    let (preview, ui) = cx.add_window_view(|window, cx| {
        let initial_focus = cx.focus_handle();
        window.focus(&initial_focus);
        Preview {
            initial_focus: Some(initial_focus),
            ..Preview::default()
        }
    });
    ui.run_until_parked();
    let choice = ui.debug_bounds("poll-choice").expect("вариант ответа");
    ui.simulate_click(choice.center(), Modifiers::none());
    ui.update(|window, app| {
        window.focus(preview.read(app).initial_focus.as_ref().unwrap());
        window.draw(app).clear();
    });
    ui.simulate_keystrokes("tab");
    ui.update(|window, app| window.draw(app).clear());
    ui.simulate_event(KeyUpEvent {
        keystroke: Keystroke::parse("enter").unwrap(),
    });
    ui.update(|_, app| {
        let expected = PollAction::Vote {
            question: "question".into(),
            option: "option".into(),
            selected: true,
        };
        assert_eq!(preview.read(app).actions, vec![expected.clone(), expected]);
    });

    for (closed, pending, read_only) in [
        (true, false, false),
        (false, true, false),
        (false, false, true),
    ] {
        ui.update(|window, app| {
            preview.update(app, |preview, cx| {
                preview.closed = closed;
                preview.pending = pending;
                preview.read_only = read_only;
                preview.actions.clear();
                cx.notify();
            });
            window.refresh();
        });
        ui.run_until_parked();
        let choice = ui.debug_bounds("poll-choice").expect("недоступный вариант");
        ui.simulate_click(choice.center(), Modifiers::none());
        ui.update(|window, app| {
            window.focus(preview.read(app).initial_focus.as_ref().unwrap());
            window.draw(app).clear();
        });
        ui.simulate_keystrokes("tab");
        ui.update(|window, app| window.draw(app).clear());
        ui.simulate_event(KeyUpEvent {
            keystroke: Keystroke::parse("enter").unwrap(),
        });
        ui.update(|_, app| assert!(preview.read(app).actions.is_empty()));
    }
}

/// Скрытые результаты не отдают аватары, а узкий бабл сохраняет числа внутри строки.
#[gpui::test]
fn result_and_voter_visibility_follow_presentation(cx: &mut TestAppContext) {
    let (preview, ui) = cx.add_window_view(|_, _| Preview::default());
    ui.simulate_resize(size(px(500.), px(480.)));
    ui.run_until_parked();
    assert!(ui.debug_bounds("poll-result-bar").is_none());
    assert!(ui.debug_bounds("poll-answer-with-result").is_none());
    assert!(ui.debug_bounds("poll-voters").is_none());
    assert_eq!(ui.debug_bounds("poll-bubble").unwrap().size.width, px(360.));

    ui.update(|_, app| {
        preview.update(app, |preview, cx| {
            preview.show_results = true;
            cx.notify();
        })
    });
    ui.simulate_resize(size(px(285.), px(480.)));
    ui.run_until_parked();

    let bubble = ui.debug_bounds("poll-bubble").unwrap();
    let answer = ui.debug_bounds("poll-answer-with-result").unwrap();
    let voters = ui.debug_bounds("poll-voters").unwrap();
    let bar = ui.debug_bounds("poll-result-bar").unwrap();
    assert!(bubble.size.width < px(285.));
    assert!(answer.size.width > px(80.));
    assert!(answer.right() <= voters.left());
    assert!(voters.right() <= bubble.right() - px(12.));
    assert_eq!(bar.size.height, px(2.));
    assert!(bar.size.width > px(0.));

    // GPUI оставляет координаты исчезнувших элементов в отладочном кадре.
    let (_, anonymous) = cx.add_window_view(|_, _| Preview {
        anonymous: true,
        show_results: true,
        ..Preview::default()
    });
    anonymous.simulate_resize(size(px(285.), px(480.)));
    anonymous.run_until_parked();
    assert!(anonymous.debug_bounds("poll-answer-with-result").is_some());
    assert!(anonymous.debug_bounds("poll-voters").is_none());
}
