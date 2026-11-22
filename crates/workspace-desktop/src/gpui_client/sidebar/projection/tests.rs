//! Проверяет правила показа каталога на фиксированных чатах и темах.

use super::*;

/// Создаёт тему с известным состоянием завершения.
fn topic(id: u128, name: &str, done: bool) -> Topic {
    let mut topic = Topic {
        id: Uuid::from_u128(id),
        name: name.into(),
        metadata: Default::default(),
    };
    topic.metadata.is_done = done;
    topic
}

/// Создаёт канал с заданным порядком тем из core.
fn channel(id: u128, name: &str, topics: Vec<Topic>) -> Chat {
    Chat {
        id: Uuid::from_u128(id),
        name: name.into(),
        direct: false,
        topics,
        metadata: Default::default(),
    }
}

/// Возвращает только названия тем, которые видит пользователь.
fn titles<'a>(group: &ChatGroup<'a>) -> Vec<&'a str> {
    group
        .topics
        .iter()
        .map(|topic| topic.name.as_str())
        .collect()
}

/// Кнопка последовательно открывает незавершённые и завершённые темы.
#[test]
fn reveals_unfinished_then_completed_and_collapses_to_first_three() {
    let chat = channel(
        1,
        "Канал",
        vec![
            topic(2, "Завершена раньше", true),
            topic(3, "Первая", false),
            topic(4, "Вторая", false),
            topic(5, "Завершена позже", true),
            topic(6, "Третья", false),
            topic(7, "Четвёртая", false),
        ],
    );
    let mut presentation = Presentation::default();
    let group = presentation.group(&chat, None).unwrap();
    assert!(!group.expanded);
    assert!(group.topics.is_empty());

    presentation.toggle_channel(chat.id);
    let group = presentation.group(&chat, None).unwrap();
    assert_eq!(titles(&group), ["Первая", "Вторая", "Третья"]);
    assert_eq!(group.toggle, Some(TopicToggleKind::ShowMore(1)));
    assert_eq!(
        presentation.apply_toggle(&chat, None, TopicToggleKind::ShowMore(1)),
        Some(TopicToggleFocus::Topic(Uuid::from_u128(7))),
    );

    let group = presentation.group(&chat, None).unwrap();
    assert_eq!(titles(&group), ["Первая", "Вторая", "Третья", "Четвёртая"]);
    assert_eq!(group.toggle, Some(TopicToggleKind::ShowCompleted(2)));
    assert_eq!(
        presentation.apply_toggle(&chat, None, TopicToggleKind::ShowCompleted(2)),
        Some(TopicToggleFocus::Topic(Uuid::from_u128(2))),
    );

    let group = presentation.group(&chat, None).unwrap();
    assert_eq!(
        titles(&group),
        [
            "Первая",
            "Вторая",
            "Третья",
            "Четвёртая",
            "Завершена раньше",
            "Завершена позже",
        ],
    );
    assert_eq!(group.toggle, Some(TopicToggleKind::HideExtra));
    assert_eq!(
        presentation.apply_toggle(&chat, None, TopicToggleKind::HideExtra),
        Some(TopicToggleFocus::Button),
    );
    assert_eq!(
        titles(&presentation.group(&chat, None).unwrap()),
        ["Первая", "Вторая", "Третья"],
    );
}

/// Завершённые входят в первые три, если незавершённых меньше.
#[test]
fn hides_only_remaining_completed_topics_and_resets_on_channel_close() {
    let chat = channel(
        1,
        "Канал",
        vec![
            topic(2, "Первая завершённая", true),
            topic(3, "Активная", false),
            topic(4, "Вторая завершённая", true),
            topic(5, "Третья завершённая", true),
        ],
    );
    let mut presentation = Presentation::default();
    presentation.ensure_expanded(chat.id);

    let group = presentation.group(&chat, None).unwrap();
    assert_eq!(
        titles(&group),
        ["Активная", "Первая завершённая", "Вторая завершённая"]
    );
    assert_eq!(group.toggle, Some(TopicToggleKind::ShowCompleted(1)));
    assert_eq!(
        presentation.apply_toggle(&chat, None, TopicToggleKind::ShowMore(1)),
        None,
    );
    assert_eq!(
        presentation.apply_toggle(&chat, None, TopicToggleKind::ShowCompleted(1)),
        Some(TopicToggleFocus::Topic(Uuid::from_u128(5))),
    );

    presentation.toggle_channel(chat.id);
    presentation.toggle_channel(chat.id);
    assert_eq!(
        presentation.group(&chat, None).unwrap().toggle,
        Some(TopicToggleKind::ShowCompleted(1)),
    );
}

/// У пустого канала и канала с тремя темами нет лишней кнопки.
#[test]
fn small_topic_lists_have_no_extra_button() {
    let full = channel(
        1,
        "Три темы",
        vec![
            topic(2, "Первая", false),
            topic(3, "Вторая", false),
            topic(4, "Третья", true),
        ],
    );
    let empty = channel(5, "Без тем", vec![]);
    let mut presentation = Presentation::default();
    presentation.ensure_expanded(full.id);
    presentation.ensure_expanded(empty.id);

    let group = presentation.group(&full, None).unwrap();
    assert!(group.expanded);
    assert_eq!(group.topics.len(), 3);
    assert_eq!(group.toggle, None);
    let group = presentation.group(&empty, None).unwrap();
    assert!(!group.expanded);
    assert_eq!(group.toggle, None);
}

/// После поиска возвращаются раскрытия обычного списка, а новый запрос сбрасывает свои.
#[test]
fn search_preserves_parent_and_expands_nonmatches_without_touching_regular_reveal() {
    let chat = channel(
        1,
        "Планирование",
        vec![
            topic(2, "Общая", false),
            topic(3, "Другие задачи", false),
            topic(4, "Окно", false),
            topic(5, "Баг сайдбара", false),
            topic(6, "САЙДБАР в архиве", true),
        ],
    );
    let mut presentation = Presentation::default();
    presentation.ensure_expanded(chat.id);
    presentation.apply_toggle(&chat, None, TopicToggleKind::ShowMore(1));
    assert!(presentation.set_query("  СаЙдБаР  "));

    let group = presentation.group(&chat, None).unwrap();
    assert!(group.expanded);
    assert_eq!(titles(&group), ["Баг сайдбара", "САЙДБАР в архиве"]);
    assert_eq!(group.toggle, Some(TopicToggleKind::ShowMore(3)));
    assert_eq!(
        presentation.apply_toggle(&chat, None, TopicToggleKind::ShowMore(3)),
        Some(TopicToggleFocus::Topic(Uuid::from_u128(2))),
    );
    assert!(!presentation.set_query("сайдбар"));
    let group = presentation.group(&chat, None).unwrap();
    assert_eq!(group.divider_before, Some(2));
    assert_eq!(
        titles(&group),
        [
            "Баг сайдбара",
            "САЙДБАР в архиве",
            "Общая",
            "Другие задачи",
            "Окно"
        ],
    );
    assert_eq!(
        presentation.apply_toggle(&chat, None, TopicToggleKind::HideExtra),
        Some(TopicToggleFocus::Button),
    );
    assert_eq!(presentation.group(&chat, None).unwrap().topics.len(), 2);
    presentation.apply_toggle(&chat, None, TopicToggleKind::ShowMore(3));

    presentation.set_query("планирование");
    let group = presentation.group(&chat, None).unwrap();
    assert!(group.topics.is_empty());
    assert_eq!(group.toggle, Some(TopicToggleKind::ShowMore(5)));
    presentation.set_query(" ");
    let group = presentation.group(&chat, None).unwrap();
    assert_eq!(
        titles(&group),
        ["Общая", "Другие задачи", "Окно", "Баг сайдбара"]
    );
    assert_eq!(group.toggle, Some(TopicToggleKind::ShowCompleted(1)));
}

/// Поиск находит личный чат по имени пользователя и сохраняет порядок групп.
#[test]
fn finds_direct_username_and_keeps_projected_catalog_order() {
    let mut dm = channel(2, "Алексей", vec![]);
    dm.direct = true;
    dm.metadata.direct_peer_username = Some("account77".into());
    dm.metadata.direct_peer_email = Some("primary@example.test".into());
    let chats = [
        channel(1, "account77 канал", vec![]),
        dm,
        channel(3, "Другой", vec![]),
    ];
    let mut presentation = Presentation::default();
    presentation.set_query("ACCOUNT77");

    let groups = presentation.project(&chats, None);
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].chat.id, chats[0].id);
    assert_eq!(groups[1].chat.id, chats[1].id);
    assert!(!groups[1].expanded);

    presentation.set_query("EXAMPLE.TEST");
    let groups = presentation.project(&chats, None);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].chat.id, chats[1].id);
}

/// Скрытие выбранной темы оставляет выделение у родительской строки.
#[test]
fn highlights_parent_when_active_topic_is_hidden() {
    let chat = channel(
        1,
        "Канал",
        vec![
            topic(2, "Первая", false),
            topic(3, "Вторая", false),
            topic(4, "Третья", false),
            topic(5, "Четвёртая", false),
        ],
    );
    let mut presentation = Presentation::default();
    presentation.ensure_expanded(chat.id);
    let selected = Some(HistoryScope {
        stream_id: chat.id,
        topic_id: Some(Uuid::from_u128(5)),
    });

    assert!(presentation.group(&chat, selected).unwrap().selected);
    presentation.apply_toggle(&chat, selected, TopicToggleKind::ShowMore(1));
    assert!(!presentation.group(&chat, selected).unwrap().selected);
    presentation.toggle_channel(chat.id);
    assert!(presentation.group(&chat, selected).unwrap().selected);
}

/// Восстановленные раскрытия принадлежат только существующим каналам.
#[test]
fn retains_only_channels_from_current_snapshot() {
    let existing = channel(1, "Канал", vec![]);
    let removed = Uuid::from_u128(2);
    let mut presentation = Presentation::default();
    presentation.set_expanded(HashSet::from([existing.id, removed]));
    let expected = HashSet::from([existing.id]);
    presentation.retain_channels(&[existing]);
    assert_eq!(presentation.expanded(), &expected);
}

/// Внешний переход раскрывает целевую завершённую тему, не меняя соседнюю группу.
#[test]
fn opens_external_topic_in_its_channel_only() {
    let first = channel(
        1,
        "Канал",
        vec![
            topic(2, "1", false),
            topic(3, "2", false),
            topic(4, "3", false),
            topic(5, "4", true),
        ],
    );
    let second = channel(
        6,
        "Другой канал",
        vec![
            topic(7, "1", false),
            topic(8, "2", false),
            topic(9, "3", false),
            topic(10, "4", true),
        ],
    );
    let mut presentation = Presentation::default();
    presentation.ensure_expanded(second.id);
    presentation.ensure_topic_visible(&first, Uuid::from_u128(5));
    let group = presentation.group(&first, None).unwrap();
    assert_eq!(titles(&group), ["1", "2", "3", "4"]);
    assert_eq!(group.toggle, Some(TopicToggleKind::HideExtra));
    let group = presentation.group(&second, None).unwrap();
    assert_eq!(titles(&group), ["1", "2", "3"]);
    assert_eq!(group.toggle, Some(TopicToggleKind::ShowCompleted(1)));
}
