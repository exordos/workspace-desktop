//! Проверяет ввод до загрузки истории и завершения проверки отправки.

use super::*;

/// Медленная история и проверка не мешают набору, а Enter не отправляет неподтверждённый текст.
#[gpui::test]
fn opening_allows_typing_before_history_and_send_confirmation(cx: &mut TestAppContext) {
    let (view, ui, requests) = start(cx);

    assert!(
        ui.debug_bounds("workspace-composer-editor").is_some(),
        "Редактор должен появиться до ответа истории"
    );
    paste(ui, "Ранний ввод");
    ui.simulate_keystrokes("enter");
    ui.run_until_parked();
    assert!(requests.save.try_recv().is_err());
    assert_eq!(draft(&view, ui), "Ранний ввод");

    publish(&requests, Load::Open(ChatEntry::Latest), vec![], vec![], ui);
    allow(&requests, Ok(TOPIC), ui);
    assert_eq!(draft(&view, ui), "Ранний ввод");
    assert!(
        requests.save.try_recv().is_err(),
        "Позднее подтверждение не отправляет текст само"
    );

    click(ui, "conversation-send");
    let queued = next(&requests.save, "отправка после подтверждения");
    assert_eq!(queued.input.topic, TOPIC);
    assert_eq!(queued.input.markdown, "Ранний ввод");
    saved(queued, None, ui);
}

/// Поздняя загрузка черновика не заменяет уже начатый ввод.
#[gpui::test]
fn late_draft_does_not_replace_new_text(cx: &mut TestAppContext) {
    let (view, ui, requests) = start(cx);
    paste(ui, "Новый текст");
    let request = next(&requests.resolve, "отложенная загрузка черновика");
    publish(&requests, Load::Open(ChatEntry::Latest), vec![], vec![], ui);
    let storage = workspace_core::storage::Storage::in_memory().unwrap();
    let scope = workspace_core::storage::DraftScope {
        stream_id: request.input.route.scope.stream_id,
        topic_id: TOPIC,
    };
    let access = storage
        .prepare_draft_access(&request.input.route.owner, scope)
        .unwrap();
    let old = workspace_core::storage::PlaintextDraft {
        id: Uuid::new_v4(),
        version: 1,
        markdown: "Сохранённый текст".into(),
        replies: None,
    };
    assert!(
        request
            .reply
            .send(ConversationReply {
                address: request.input,
                value: Ok(crate::gpui_client::composer::ResolvedDestination {
                    topic: TOPIC,
                    encrypted: false,
                    closed: false,
                    plain_target: None,
                    restored_protected: false,
                    draft: Some((access, Some(old))),
                }),
            })
            .is_ok()
    );
    ui.run_until_parked();
    assert_eq!(draft(&view, ui), "Новый текст");
}

/// Отзыв темы лички удаляет ранний ввод и не позволяет старому ответу его вернуть.
#[gpui::test]
fn revoked_default_topic_clears_early_input_before_destination_reply(cx: &mut TestAppContext) {
    use workspace_core::{
        messenger::catalog::{Chat, ChatMetadata},
        sync::HistoryRevocation,
    };

    let (view, ui, requests) = start(cx);
    ui.update(|_, app| {
        view.update(app, |view, cx| {
            let chat = Chat {
                id: view.address().route.scope.stream_id,
                name: "Личка".into(),
                direct: true,
                topics: vec![],
                metadata: ChatMetadata {
                    default_topic_id: Some(TOPIC),
                    encrypted: Some(false),
                    ..Default::default()
                },
            };
            view.set_new_chat_heading(&chat, cx);
        })
    });
    paste(ui, "Ранний ввод отозванной темы");
    let request = next(&requests.resolve, "задержанная проверка цели");
    publish(&requests, Load::Open(ChatEntry::Latest), vec![], vec![], ui);
    let stream_id = request.input.route.scope.stream_id;
    changed(
        &view,
        ui,
        SyncChange::HistoryAccessRevoked(HistoryRevocation::Topic {
            stream_id,
            topic_id: TOPIC,
        }),
    );
    assert!(
        draft(&view, ui).is_empty(),
        "Отозванный ввод должен исчезнуть сразу"
    );
    assert!(
        request
            .reply
            .send(ConversationReply {
                address: request.input,
                value: Ok(crate::gpui_client::composer::ResolvedDestination {
                    topic: TOPIC,
                    encrypted: false,
                    closed: false,
                    plain_target: None,
                    restored_protected: false,
                    draft: None,
                }),
            })
            .is_ok()
    );
    ui.run_until_parked();
    assert!(draft(&view, ui).is_empty());
    publish(&requests, Load::Open(ChatEntry::Latest), vec![], vec![], ui);
    allow(&requests, Ok(Uuid::from_u128(2003)), ui);
    assert!(draft(&view, ui).is_empty());
}
