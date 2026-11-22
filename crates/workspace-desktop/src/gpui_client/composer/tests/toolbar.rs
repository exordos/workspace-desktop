//! Проверяет раскрытие и редактирование через рабочее окно разговора.

use super::*;

/// Раскрытие сохраняет размеры поля, а форматирование сохраняет Unicode и защищает IME.
#[gpui::test]
fn toolbar_formats_text_without_submitting_or_disturbing_ime(cx: &mut TestAppContext) {
    let (view, ui, requests) = start(cx);
    publish(&requests, Load::Open(ChatEntry::Latest), vec![], vec![], ui);
    allow(&requests, Ok(TOPIC), ui);
    paste(ui, "Отбор 🧩");
    ui.simulate_keystrokes("cmd-a");
    let compact_field = ui.debug_bounds("conversation-composer-field").unwrap();
    let compact_send = ui.debug_bounds("conversation-send").unwrap();
    let compact_toggle = ui.debug_bounds("composer-toolbar-toggle").unwrap();
    let compact_editor = ui.debug_bounds("workspace-composer-editor").unwrap();
    assert_eq!(compact_field.size.height, px(40.));
    assert_eq!(compact_send.size, size(px(40.), px(40.)));
    assert_eq!(compact_send.left() - compact_field.right(), px(12.));
    assert_eq!(compact_editor.left() - compact_field.left(), px(48.));
    assert!(ui.debug_bounds("composer-toolbar").is_none());

    click(ui, "composer-toolbar-toggle");
    let composer = ui.debug_bounds("conversation-composer").unwrap();
    let toolbar = ui.debug_bounds("composer-toolbar").unwrap();
    let field = ui.debug_bounds("conversation-composer-field").unwrap();
    let send = ui.debug_bounds("conversation-send").unwrap();
    let toggle = ui.debug_bounds("composer-toolbar-toggle").unwrap();
    let editor_bounds = ui.debug_bounds("workspace-composer-editor").unwrap();
    assert_eq!(field.size.height, compact_field.size.height);
    assert_eq!(send.size, compact_send.size);
    assert_eq!(field.top() - composer.top(), px(8.));
    assert_eq!(field.left() - composer.left(), px(8.));
    assert_eq!(composer.right() - send.right(), px(8.));
    assert_eq!(composer.bottom() - toolbar.bottom(), px(8.));
    assert_eq!(send.left() - field.right(), px(12.));
    assert_eq!(editor_bounds.left() - field.left(), px(20.));
    assert_eq!(toolbar.left(), field.left());
    assert_eq!(toolbar.right(), field.right());
    assert_eq!(toolbar.top() - field.bottom(), px(4.));
    assert_eq!(toolbar.size.height, px(32.));
    assert_eq!(toggle.left() - toolbar.left(), px(8.));
    assert_eq!(toggle.left(), compact_toggle.left());
    assert_eq!(toggle.size, size(px(32.), px(32.)));

    // В трёх группах отступы 0 и 8 px, а разделители добавляют 1 px между группами.
    let commands = [
        "composer-format-link",
        "composer-format-bold",
        "composer-format-italic",
        "composer-format-strike",
        "composer-format-numbered",
        "composer-format-bulleted",
        "composer-format-quote",
        "composer-format-block",
        "composer-format-inline",
        "composer-format-spoiler",
    ]
    .map(|selector| ui.debug_bounds(selector).unwrap());
    assert_eq!(commands[0].left() - toggle.right(), px(17.));
    for command in &commands {
        assert_eq!(command.size, size(px(32.), px(32.)));
        assert_eq!(command.top(), toolbar.top());
    }
    for (pair, gap) in commands
        .windows(2)
        .zip([0., 0., 0., 17., 8., 17., 8., 8., 8.])
    {
        assert_eq!(pair[1].left() - pair[0].right(), px(gap));
    }

    click(ui, "composer-format-bold");
    assert_eq!(draft(&view, ui), "**Отбор 🧩**");
    ui.simulate_keystrokes("shift-enter h");
    assert_eq!(draft(&view, ui), "**Отбор 🧩**\nh");
    let multiline_field = ui.debug_bounds("conversation-composer-field").unwrap();
    let multiline_send = ui.debug_bounds("conversation-send").unwrap();
    assert_eq!(multiline_field.size.height, px(64.));
    assert_eq!(multiline_send.bottom(), multiline_field.bottom());
    assert_eq!(multiline_send.size, compact_send.size);

    ui.simulate_keystrokes("cmd-a");
    click(ui, "composer-format-link");
    ui.simulate_keystrokes("x");
    assert_eq!(draft(&view, ui), "[**Отбор 🧩**\nh](x)");

    paste(ui, "  3. Тест🙂");
    ui.simulate_keystrokes("shift-enter");
    assert_eq!(draft(&view, ui), "  3. Тест🙂\n  4. ");
    ui.simulate_keystrokes("shift-enter");
    assert_eq!(draft(&view, ui), "  3. Тест🙂\n");
    let editor = ui.read(|app| view.read(app).tested_composer().unwrap().read(app).editor());
    ui.update(|window, app| {
        editor.update(app, |field, cx| {
            field.replace_and_mark_text_in_range(None, "仮", Some(0..1), window, cx);
        });
    });
    ui.run_until_parked();
    click(ui, "composer-format-bold");
    ui.simulate_keystrokes("cmd-i");
    assert_eq!(draft(&view, ui), "  3. Тест🙂\n仮");
    assert!(ui.update(|window, app| {
        editor.update(app, |field, cx| {
            field.marked_text_range(window, cx).is_some()
        })
    }));
    ui.update(|window, app| {
        editor.update(app, |field, cx| {
            field.replace_text_in_range(None, "確定", window, cx);
        });
    });
    ui.run_until_parked();
    click(ui, "composer-toolbar-toggle");
    assert_eq!(
        ui.debug_bounds("conversation-send").unwrap().size,
        size(px(40.), px(40.))
    );
    assert_eq!(draft(&view, ui), "  3. Тест🙂\n確定");
    assert!(requests.save.try_recv().is_err());
}

/// Находит правую команду клавиатурой и не смещает читаемую строку истории.
#[gpui::test]
fn narrow_toolbar_scrolls_focused_actions_and_keeps_reading_anchor(cx: &mut TestAppContext) {
    let (view, ui, requests) = start(cx);
    ui.simulate_resize(size(px(340.), px(530.)));
    let stream_id = ui.read(|app| view.read(app).address().route.scope.stream_id);
    let messages = (1..=60)
        .map(|index| HistoryMessage {
            id: Uuid::from_u128(index),
            stream_id,
            topic_id: TOPIC,
            text: "Синтетическая строка".into(),
            metadata: HistoryMetadata {
                author_id: Some(Uuid::from_u128(900)),
                created_at: HistoryTime::parse("2026-11-17T07:10:30Z").unwrap(),
                updated_at: None,
            },
        })
        .collect();
    publish(
        &requests,
        Load::Open(ChatEntry::Latest),
        messages,
        vec![],
        ui,
    );
    allow(&requests, Ok(TOPIC), ui);
    ui.update(|_, app| {
        view.update(app, |view, cx| {
            view.viewport.list().scroll_to(ListOffset {
                item_ix: 8,
                offset_in_item: px(11.),
            });
            cx.notify();
        });
    });
    ui.run_until_parked();
    let y = |ui: &VisualTestContext| {
        ui.read(|app| view.read(app).viewport.y(Uuid::from_u128(9)).unwrap())
    };
    let initial_y = y(ui);
    paste(ui, "один\nдва");
    ui.simulate_keystrokes("cmd-a");
    click(ui, "composer-toolbar-toggle");
    assert_eq!(y(ui), initial_y);
    let viewport = ui.debug_bounds("composer-toolbar").unwrap();
    let offscreen = ui.debug_bounds("composer-format-spoiler").unwrap();
    assert!(offscreen.right() > viewport.right());
    assert_eq!(
        viewport.right(),
        ui.debug_bounds("conversation-composer-field")
            .unwrap()
            .right()
    );
    let send = ui.debug_bounds("conversation-send").unwrap();
    assert!(send.right() <= px(340.));

    // Те же переходы вызывает Tab в корне штатного окна.
    for _ in 0..12 {
        ui.update(|window, _| window.focus_next());
        ui.run_until_parked();
    }
    let visible = ui.debug_bounds("composer-format-spoiler").unwrap();
    assert!(
        visible.left() >= viewport.left() && visible.right() <= viewport.right(),
        "viewport={viewport:?} visible={visible:?}"
    );
    ui.simulate_keystrokes("enter");
    ui.simulate_event(KeyUpEvent {
        keystroke: Keystroke::parse("enter").unwrap(),
    });
    ui.run_until_parked();
    assert_eq!(draft(&view, ui), "||один\nдва||");
    assert!(requests.save.try_recv().is_err());
    assert_eq!(y(ui), initial_y);

    // Обход в обратную сторону возвращает левую кнопку в область прокрутки.
    for _ in 0..2 {
        ui.update(|window, _| window.focus_next());
        ui.run_until_parked();
    }
    click(ui, "composer-toolbar-toggle");
    let field = ui.debug_bounds("conversation-composer-field").unwrap();
    let toggle = ui.debug_bounds("composer-toolbar-toggle").unwrap();
    assert_eq!(field.size.height, px(64.));
    assert_eq!(field.bottom() - toggle.bottom(), px(4.));
    assert_eq!(
        ui.debug_bounds("conversation-send").unwrap().size,
        size(px(40.), px(40.))
    );
    assert_eq!(draft(&view, ui), "||один\nдва||");
    assert_eq!(y(ui), initial_y);
}

/// При сужении уже сфокусированная команда остаётся видимой без повторного Tab.
#[gpui::test]
fn toolbar_keeps_focused_action_visible_after_resize(cx: &mut TestAppContext) {
    let (_, ui, requests) = start(cx);
    publish(&requests, Load::Open(ChatEntry::Latest), vec![], vec![], ui);
    allow(&requests, Ok(TOPIC), ui);
    paste(ui, "Сообщение");
    click(ui, "composer-toolbar-toggle");
    for _ in 0..12 {
        ui.update(|window, _| window.focus_next());
        ui.run_until_parked();
    }
    let focused = ui.update(|window, app| window.focused(app).unwrap());
    let visible = |ui: &mut VisualTestContext| {
        let viewport = ui.debug_bounds("composer-toolbar").unwrap();
        let action = ui.debug_bounds("composer-format-spoiler").unwrap();
        action.left() >= viewport.left() && action.right() <= viewport.right()
    };
    assert!(visible(ui));
    ui.simulate_resize(size(px(340.), px(530.)));
    ui.run_until_parked();
    assert!(ui.update(|window, app| window.focused(app).as_ref() == Some(&focused)));
    assert!(visible(ui));
}
