//! Показывает состояния опроса на демонстрационных данных в обеих палитрах.

use gpui::{Context, Div, SharedString, div, prelude::*, px};
use workspace_desktop::gpui_client::{
    Language,
    ui::{
        Avatar, Button, DeliveryMark, MessageBubble, Palette, TextRole, label,
        poll::{PollAction, PollAddOption, PollBubble, PollOption, PollQuestion, PollResult},
    },
};

use super::Panel;

/// Хранит изменения пользователя только внутри этой панели галереи.
#[derive(Default)]
pub(super) struct Examples {
    choice: Option<SharedString>, // Выбранный вариант первого примера.
    expanded: bool,               // Показывать ли опрос из 20 вопросов по 20 вариантов.
}

impl Examples {
    /// Расставляет состояния опроса рядом с первым интерактивным примером.
    pub(super) fn render(&self, palette: Palette, cx: &Context<Panel>) -> Div {
        let panel = cx.entity().downgrade();
        let boundary_target = panel.clone();
        let mut schedule = schedule(
            self.choice.is_some(),
            self.choice.as_ref().map(SharedString::as_str),
        );
        let total = 24 + u64::from(self.choice.is_some());
        for option in &mut schedule.options {
            if let Some(result) = &mut option.result {
                result.votes += u64::from(option.selected);
                result.percent = ((result.votes * 100 + total / 2) / total) as u8;
            }
        }
        let interactive = PollBubble {
            participants: self.choice.is_some().then_some(total),
            ..bubble("poll-interactive", vec![schedule])
        }
        .render(
            message(false),
            palette,
            Language::Ru,
            false,
            move |action, _, app| {
                if let PollAction::Vote {
                    option, selected, ..
                } = action
                {
                    let _ = panel.update(app, |panel, cx| {
                        panel.polls.choice = selected.then_some(option);
                        cx.notify();
                    });
                }
            },
        );
        let boundary_label = if self.expanded {
            "Свернуть опрос 20 × 20"
        } else {
            "Показать опрос 20 × 20"
        };

        div()
            .flex()
            .flex_col()
            .min_w_0()
            .gap_3()
            .child(label(palette, TextRole::Heading, "Опросы"))
            .child(scene(palette, "Одиночный выбор", interactive))
            .children(
                fixtures()
                    .into_iter()
                    .map(|(title, poll, own)| fixed_scene(palette, title, poll, own)),
            )
            .child(narrow_scene(palette))
            .child(Button::new("gallery-poll-boundary", boundary_label).render(
                palette,
                None,
                move |_, app| {
                    let _ = boundary_target.update(app, |panel, cx| {
                        panel.polls.expanded = !panel.polls.expanded;
                        cx.notify();
                    });
                },
            ))
            .when(self.expanded, |section| {
                section.child(fixed_scene(
                    palette,
                    "Предельное число вопросов и вариантов",
                    boundary(),
                    false,
                ))
            })
    }
}

/// Создаёт опрос с обычным открытым состоянием и переданными вопросами.
fn bubble(id: &'static str, questions: Vec<PollQuestion>) -> PollBubble {
    PollBubble {
        id: id.into(),
        anonymous: false,
        multiple_answers: false,
        closed: false,
        read_only: false,
        participants: None,
        view_votes: false,
        status_date: None,
        error: None,
        questions,
    }
}

/// Создаёт вопрос с доступными вариантами для примера.
fn question(
    id: impl Into<SharedString>,
    text: impl Into<SharedString>,
    options: Vec<PollOption>,
) -> PollQuestion {
    PollQuestion {
        id: id.into(),
        text: text.into(),
        participants: None,
        pending: false,
        error: None,
        correct_answer: None,
        reserve_correct_answer: false,
        add_option: None,
        options,
    }
}

/// Даёт варианту текст и при необходимости показывает готовые результаты.
fn option(
    id: impl Into<SharedString>,
    text: impl Into<SharedString>,
    result: Option<PollResult>,
) -> PollOption {
    PollOption {
        id: id.into(),
        text: text.into(),
        selected: false,
        correctness: None,
        enabled: true,
        result,
        voters: Vec::new(),
    }
}

/// Повторяет вопрос про встречу из Storybook с выбором и видимостью итогов.
fn schedule(show_results: bool, selection: Option<&str>) -> PollQuestion {
    let options = [
        ("tuesday", "Вторник, 11:00", 6, 25),
        ("wednesday", "Среда, 14:00", 16, 67),
        ("thursday", "Четверг, 16:00", 2, 8),
    ]
    .map(|(id, text, votes, percent)| PollOption {
        selected: selection == Some(id),
        ..option(
            id,
            text,
            show_results.then_some(PollResult { votes, percent }),
        )
    });
    question(
        "schedule",
        "Когда проведём встречу команды?",
        options.into(),
    )
}

/// Повторяет вопрос с множественным выбором из Storybook.
fn topics(answered: bool) -> PollQuestion {
    let options = [
        ("plans", "Планы команды", 12, 60, true),
        ("results", "Результаты проекта", 8, 40, false),
        ("feedback", "Обратная связь", 6, 30, true),
    ]
    .map(|(id, text, votes, percent, chosen)| PollOption {
        selected: answered && chosen,
        ..option(id, text, answered.then_some(PollResult { votes, percent }))
    });
    PollQuestion {
        participants: answered.then_some(20),
        ..question("topics", "Какие темы обсудим?", options.into())
    }
}

/// Заполняет доступные превью участников инициалами из демонстрационных данных.
fn give_voter_previews(question: &mut PollQuestion) {
    const INITIALS: [&str; 5] = ["А", "Б", "В", "Г", "Д"];
    for option in &mut question.options {
        if let Some(result) = &option.result {
            option.voters = INITIALS
                .into_iter()
                .take(result.votes.min(5) as usize)
                .map(Avatar::from_initials)
                .collect();
        }
    }
}

/// Готовит независимые состояния из Storybook и дополнительные подписи ошибок и срока.
fn fixtures() -> Vec<(&'static str, PollBubble, bool)> {
    let independent = PollBubble {
        anonymous: true,
        multiple_answers: true,
        ..bubble(
            "poll-independent",
            vec![
                PollQuestion {
                    participants: Some(24),
                    ..schedule(true, Some("wednesday"))
                },
                topics(false),
            ],
        )
    };
    let anonymous_answered = PollBubble {
        anonymous: true,
        multiple_answers: true,
        participants: Some(26),
        ..bubble(
            "poll-anonymous",
            vec![
                PollQuestion {
                    participants: Some(24),
                    ..schedule(true, Some("wednesday"))
                },
                topics(true),
            ],
        )
    };
    let anonymous_open = PollBubble {
        anonymous: true,
        participants: Some(24),
        ..bubble("poll-anonymous-open", vec![schedule(true, None)])
    };
    let public = PollBubble {
        participants: Some(24),
        view_votes: true,
        status_date: Some("12 ноября, 18:00".into()),
        ..bubble(
            "poll-public",
            vec![{
                let mut question = schedule(true, Some("wednesday"));
                give_voter_previews(&mut question);
                for option in &mut question.options {
                    option.enabled = false;
                }
                question
            }],
        )
    };
    let closed = PollBubble {
        closed: true,
        participants: Some(24),
        status_date: Some("24 ноября, 13:15".into()),
        ..bubble("poll-closed", vec![schedule(true, Some("wednesday"))])
    };
    let snapshot = PollBubble {
        read_only: true,
        participants: Some(24),
        ..bubble("poll-snapshot", vec![schedule(true, Some("wednesday"))])
    };
    let pending = PollBubble {
        multiple_answers: true,
        ..bubble(
            "poll-pending",
            vec![
                PollQuestion {
                    pending: true,
                    ..schedule(false, None)
                },
                PollQuestion {
                    add_option: Some(PollAddOption {
                        text: "Доступность нового экрана".into(),
                        pending: true,
                        error: false,
                    }),
                    ..topics(false)
                },
            ],
        )
    };
    let vote_error = bubble(
        "poll-vote-error",
        vec![PollQuestion {
            error: Some("Соединение потеряно. Повторите попытку.".into()),
            ..schedule(false, None)
        }],
    );
    let poll_error = PollBubble {
        error: Some("Не удалось обновить результаты.".into()),
        ..bubble("poll-error", vec![schedule(false, None)])
    };
    let additions = PollBubble {
        multiple_answers: true,
        status_date: Some("30 ноября, 18:00".into()),
        ..bubble(
            "poll-additions",
            vec![
                PollQuestion {
                    add_option: Some(PollAddOption {
                        text: "".into(),
                        pending: false,
                        error: false,
                    }),
                    ..schedule(false, None)
                },
                PollQuestion {
                    add_option: Some(PollAddOption {
                        text: "Доступность нового экрана".into(),
                        pending: false,
                        error: true,
                    }),
                    ..topics(false)
                },
            ],
        )
    };

    vec![
        (
            "Несколько вопросов с независимым выбором",
            independent,
            false,
        ),
        ("Анонимные результаты до голосования", anonymous_open, false),
        (
            "Анонимные результаты после голосования",
            anonymous_answered,
            true,
        ),
        ("Публичные результаты и превью участников", public, false),
        ("Завершённый опрос", closed, false),
        ("Снимок для чтения", snapshot, true),
        ("Сохранение ответа и нового варианта", pending, false),
        ("Ошибка ответа", vote_error, false),
        ("Общая ошибка опроса", poll_error, false),
        (
            "Опрос недоступен",
            bubble("poll-unavailable", Vec::new()),
            false,
        ),
        ("Викторина до ответа", unanswered_quiz(), false),
        (
            "Викторина с правильным и неправильным ответом",
            answered_quiz(),
            false,
        ),
        ("Добавление вариантов и срок", additions, false),
    ]
}

/// Показывает правильный и неправильный выбор вместе с итогами викторины.
fn answered_quiz() -> PollBubble {
    let mut schedule = schedule(true, Some("tuesday"));
    schedule.options[0].correctness = Some(true);
    schedule.correct_answer = Some("Вторник, 11:00".into());
    schedule.participants = Some(24);
    let mut topics = topics(true);
    topics.options[0].correctness = Some(true);
    topics.options[2].correctness = Some(false);
    topics.correct_answer = Some("Планы команды".into());
    PollBubble {
        anonymous: true,
        multiple_answers: true,
        participants: Some(26),
        ..bubble("poll-quiz-answered", vec![schedule, topics])
    }
}

/// Оставляет место под подпись верного ответа, пока пользователь выбирает вариант.
fn unanswered_quiz() -> PollBubble {
    PollBubble {
        anonymous: true,
        ..bubble(
            "poll-quiz-unanswered",
            vec![PollQuestion {
                reserve_correct_answer: true,
                ..schedule(false, None)
            }],
        )
    }
}

/// Проверяет перенос вопроса, подписи и результатов при ширине меньше 300 px.
fn narrow_scene(palette: Palette) -> Div {
    let mut question = schedule(true, Some("wednesday"));
    question.text =
        "Какой слот подойдёт для обсуждения результатов синхронизации и следующих шагов?".into();
    question.options[0].text =
        "Вторник, после общей встречи по планированию работы на следующий месяц".into();
    for option in &mut question.options {
        option.result = Some(PollResult {
            votes: 80,
            percent: 100,
        });
    }
    give_voter_previews(&mut question);
    let poll = PollBubble {
        participants: Some(80),
        multiple_answers: true,
        view_votes: true,
        ..bubble("poll-narrow", vec![question])
    };
    div().max_w(px(280.)).child(scene(
        palette,
        "Узкий бабл, много участников, сообщение выделено",
        poll.render(message(false), palette, Language::Ru, true, |_, _, _| {}),
    ))
}

/// Создаёт 20 вопросов с 20 вариантами только по запросу в галерее.
fn boundary() -> PollBubble {
    let questions = (1..=20)
        .map(|index| {
            question(
                format!("question-{index}"),
                format!("Вопрос {index}"),
                (1..=20)
                    .map(|index| {
                        option(format!("option-{index}"), format!("Вариант {index}"), None)
                    })
                    .collect(),
            )
        })
        .collect();
    PollBubble {
        multiple_answers: true,
        ..bubble("poll-boundary", questions)
    }
}

/// Добавляет автора, время и отметку о прочтении к примеру сообщения.
fn message(own: bool) -> MessageBubble {
    MessageBubble {
        author: (!own).then(|| "Алексей · Общий".into()),
        body: "".into(),
        time: Some("15:40".into()),
        own,
        delivery: own.then_some(DeliveryMark::Read),
    }
}

/// Рисует фиксированный опрос в том же пузыре, который используется в первом примере.
fn fixed_scene(palette: Palette, title: &'static str, poll: PollBubble, own: bool) -> Div {
    let contents = poll.render(message(own), palette, Language::Ru, false, |_, _, _| {});
    scene(palette, title, contents)
}

/// Подписывает состояние для быстрого поиска в списке примеров.
fn scene(palette: Palette, title: &'static str, contents: Div) -> Div {
    div()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_1()
        .child(label(palette, TextRole::Label, title))
        .child(contents)
}
