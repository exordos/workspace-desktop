//! Рисует бабл опроса из готовых данных. Сначала описаны состояния и действия,
//! затем оболочка сообщения, вопросы, результаты и строка добавления ответа.

use std::{borrow::Cow, rc::Rc};

use gpui::{
    App, Div, FontWeight, Hsla, SharedString, Stateful, Window, div, prelude::*, px, relative, rgb,
    transparent_black,
};

use super::{Avatar, Icon, MessageBubble, Palette, activate, theme::PANEL_RADIUS};
use crate::gpui_client::Language;

const POLL_FONT: &str = "Montserrat";
const MAX_VOTER_AVATARS: usize = 5;
const VOTER_AVATAR_SIZE: f32 = 28.;
const VOTER_AVATAR_OVERLAP: f32 = 8.;

/// Данные для отображения опроса в сообщении.
pub struct PollBubble {
    pub id: SharedString,          // Стабильный ключ опроса в списке сообщений.
    pub anonymous: bool,           // Вариант без показа участников.
    pub multiple_answers: bool,    // Подпись о возможности выбрать несколько ответов.
    pub closed: bool,              // Опрос завершён, в том числе по истечении срока.
    pub read_only: bool,           // Снимок для чтения с отдельной подписью.
    pub participants: Option<u64>, // Видимое общее число участников, включая ноль.
    pub view_votes: bool,          // На сводке доступен переход к списку голосов.
    pub status_date: Option<SharedString>, // Готовая локальная дата срока или завершения.
    pub error: Option<SharedString>, // Ошибка, относящаяся ко всему опросу.
    pub questions: Vec<PollQuestion>, // Вопросы в порядке показа, пустой список — данные недоступны.
}

/// Вопрос с подготовленным состоянием ответов.
pub struct PollQuestion {
    pub id: SharedString,                     // Ключ вопроса внутри опроса.
    pub text: SharedString,                   // Заголовок вопроса.
    pub participants: Option<u64>,            // Видимое число участников этого вопроса.
    pub pending: bool, // Действие для вопроса выполняется, менять выбор нельзя.
    pub error: Option<SharedString>, // Ошибка действия под конкретным вопросом.
    pub correct_answer: Option<SharedString>, // Видимый список правильных ответов через запятую.
    pub reserve_correct_answer: bool, // Оставить место под скрытый ответ викторины.
    pub add_option: Option<PollAddOption>, // Показать строку добавления варианта.
    pub options: Vec<PollOption>, // Варианты в порядке показа.
}

/// Вариант ответа с уже подготовленными данными о выборе и итогах.
pub struct PollOption {
    pub id: SharedString,           // Ключ варианта внутри вопроса.
    pub text: SharedString,         // Текст ответа.
    pub selected: bool,             // Текущий собственный выбор.
    pub correctness: Option<bool>,  // Some(true) — верный, Some(false) — выбранный неверный.
    pub enabled: bool,              // Этому варианту разрешено изменить выбор.
    pub result: Option<PollResult>, // None скрывает числа, полосу и аватары.
    pub voters: Vec<Avatar>,        // Видимые аватары участников в порядке показа.
}

/// Числа рядом с ответом и длина полосы результата.
pub struct PollResult {
    pub votes: u64,  // Число голосов за вариант.
    pub percent: u8, // Доля участников вопроса в целых процентах.
}

/// Внешний вид строки добавления варианта.
pub struct PollAddOption {
    pub text: SharedString, // Текущий видимый текст, пустой показывает подсказку.
    pub pending: bool,      // Строка ждёт окончания сохранения.
    pub error: bool,        // Под строкой нужна подпись об ошибке.
}

/// Намерение пользователя, с которым работает вызывающий экран.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PollAction {
    /// Изменить отметку варианта.
    Vote {
        question: SharedString, // Ключ вопроса.
        option: SharedString,   // Ключ варианта.
        selected: bool,         // Какую отметку запросил пользователь.
    },
    /// Открыть список участников по варианту.
    Voters {
        question: SharedString, // Ключ вопроса.
        option: SharedString,   // Ключ варианта.
    },
    Overview, // Открыть сводку голосов.
    /// Перейти к вводу нового варианта.
    AddOption {
        question: SharedString, // Ключ вопроса.
    },
}

type OnAction = Rc<dyn Fn(PollAction, &mut Window, &mut App)>;

/// Параметры, общие для вопросов и вариантов одного бабла.
struct PollStyle {
    palette: Palette,    // Тема активного окна.
    language: Language,  // Язык подписей опроса.
    can_vote: bool,      // Бабл открыт для действий с ответами.
    show_voters: bool,   // Видим список участников публичного опроса.
    on_action: OnAction, // Передаёт действие родительскому экрану.
}

impl PollBubble {
    /// Загружает шрифт оригинального бабла при запуске окна компонентов.
    pub fn load_font(app: &App) {
        let font = include_bytes!("../../../assets/fonts/poll/montserrat-regular.ttf");
        if let Err(error) = app.text_system().add_fonts(vec![Cow::Borrowed(font)]) {
            log::warn!("Не удалось загрузить шрифт опросов: {error}");
        }
    }

    /// Собирает бабл шириной до 360 px с общей подписью времени и статусом доставки.
    pub fn render(
        self,
        message: MessageBubble,
        palette: Palette,
        language: Language,
        selected: bool,
        on_action: impl Fn(PollAction, &mut Window, &mut App) + 'static,
    ) -> Div {
        let style = PollStyle {
            palette,
            language,
            can_vote: !self.closed && !self.read_only,
            show_voters: !self.anonymous && !self.read_only,
            on_action: Rc::new(on_action),
        };
        let subtitle = self.subtitle(language);
        let quiz = self.questions.iter().any(|q| q.correct_answer.is_some());
        let metadata = message.metadata(
            Palette {
                message_time: palette.text_muted,
                ..palette
            },
            false,
        );
        // Оставляем место справа на последней строке, как в бабле Electron.
        let reserve = if metadata.is_none() {
            0.
        } else if message.own && message.delivery.is_some() {
            68.
        } else {
            48.
        };
        let has_status = self.status_date.is_some() || self.read_only || self.error.is_some();
        let summary = self.summary(quiz, &style);
        let status = self.status(&style);
        let question_count = self.questions.len();
        let bubble = div()
            .id(self.id)
            .relative()
            .w(px(360.))
            .max_w(relative(0.88))
            .min_w_0()
            .rounded(PANEL_RADIUS)
            .px(px(12.))
            .py(px(8.))
            .bg(if selected {
                palette.card_selected
            } else {
                palette.surface
            })
            .font_family(POLL_FONT)
            .text_size(px(12.))
            .line_height(px(15.))
            .text_color(palette.text)
            .debug_selector(|| "poll-bubble".into())
            .children(message.author_label(palette).map(|author| {
                author
                    .font_family("Helvetica Neue")
                    .text_size(px(12.))
                    .line_height(px(15.))
                    .mb(px(6.))
            }));
        let bubble = if question_count == 0 {
            bubble.child(
                div()
                    .min_w_0()
                    .min_h(px(16.))
                    .pr(px(reserve))
                    .font_family("Helvetica Neue")
                    .text_size(px(14.))
                    .line_height(px(20.))
                    .child(language.tr("poll_bubble.unavailable")),
            )
        } else {
            bubble
                .children(
                    self.questions
                        .into_iter()
                        .enumerate()
                        .map(|(index, question)| {
                            let subtitle = (index == 0).then_some(subtitle.as_str());
                            question.render(index, question_count, subtitle, &style)
                        }),
                )
                .child(summary.when(!has_status && reserve > 0., |line| {
                    line.pr(px(reserve)).min_h(px(16.))
                }))
                .when(has_status, |bubble| {
                    bubble.child(
                        status.when(reserve > 0., |line| line.pr(px(reserve)).min_h(px(16.))),
                    )
                })
        };
        message.align(
            bubble.children(metadata.map(|time| time.absolute().right(px(12.)).bottom(px(8.)))),
        )
    }

    /// Показывает вид опроса и режим выбора либо подпись о завершении.
    fn subtitle(&self, language: Language) -> String {
        if self.closed {
            return language.tr("poll_bubble.closed").into();
        }
        let privacy = language.tr(if self.anonymous {
            "poll_bubble.anonymous"
        } else {
            "poll_bubble.public"
        });
        let choice = language.tr(if self.multiple_answers {
            "poll_bubble.multiple"
        } else {
            "poll_bubble.single"
        });
        let mut subtitle = format!("{privacy} · {choice}");
        if self.questions.len() > 1 {
            let count = self.questions.len().to_string();
            subtitle.push_str(" · ");
            subtitle.push_str(&language.tr_with("poll_bubble.questions", &[("count", &count)]));
        }
        subtitle
    }

    /// Показывает итоги на последней строке и открывает сводку там, где она доступна.
    fn summary(&self, quiz: bool, style: &PollStyle) -> Div {
        let line = status_line(style.palette);
        let Some(count) = self.participants else {
            return line.child(style.language.tr("poll_bubble.results_after_vote"));
        };
        let label = style
            .language
            .plural("poll_bubble.participant_count", count);
        if quiz {
            return line.child(format!(
                "{label} · {}",
                style.language.tr("poll_bubble.you_voted")
            ));
        }
        if self.view_votes && style.show_voters && !self.closed {
            let label = format!("{label} · {}", style.language.tr("poll_bubble.view_votes"));
            let link = div()
                .id("poll-overview")
                .text_color(style.palette.accent)
                .hover(|style| style.underline())
                .child(label);
            return line.child(poll_control(link, true, PollAction::Overview, style));
        }
        line.child(label)
    }

    /// Собирает видимые подписи срока, режима чтения и ошибки опроса.
    fn status(&self, style: &PollStyle) -> Div {
        let date_key = if self.closed {
            "poll_bubble.completed_at"
        } else {
            "poll_bubble.deadline"
        };
        status_line(style.palette)
            .flex()
            .flex_wrap()
            .gap_x(px(8.))
            .gap_y(px(4.))
            .children(
                self.status_date
                    .as_ref()
                    .map(|date| style.language.tr_with(date_key, &[("date", date.as_ref())])),
            )
            .children(
                self.read_only
                    .then(|| style.language.tr("poll_bubble.read_only")),
            )
            .children(self.error.as_ref().map(|error| {
                div()
                    .min_w_0()
                    .max_w_full()
                    .text_color(style.palette.danger)
                    .child(error.clone())
            }))
    }
}

impl PollQuestion {
    /// Рисует заголовок, независимые варианты ответа и подписи вопроса.
    fn render(
        self,
        index: usize,
        total: usize,
        subtitle: Option<&str>,
        style: &PollStyle,
    ) -> Stateful<Div> {
        let palette = style.palette;
        let can_vote = style.can_vote && !self.pending;
        div()
            .id(self.id.clone())
            .min_w_0()
            .when(index > 0, |question| {
                question
                    .mt(px(12.))
                    .pt(px(12.))
                    .border_t_1()
                    .border_color(palette.border)
            })
            .child(
                div()
                    .font_family("Helvetica Neue")
                    .font_weight(FontWeight::MEDIUM)
                    .text_size(px(14.))
                    .line_height(px(17.))
                    .mb(px(6.))
                    .child(self.text),
            )
            .children(subtitle.map(|subtitle| {
                div()
                    .mb(px(6.))
                    .text_color(palette.text_secondary)
                    .child(subtitle.to_owned())
            }))
            .child(
                div().flex().flex_col().gap(px(6.)).min_w_0().children(
                    self.options
                        .into_iter()
                        .map(|option| option.render(&self.id, can_vote, style)),
                ),
            )
            .when(
                self.reserve_correct_answer || self.correct_answer.is_some(),
                |question| {
                    question.child(
                        status_line(palette)
                            .min_h(px(15.))
                            .text_color(palette.presence_online)
                            .children(self.correct_answer.map(|answer| {
                                format!(
                                    "✓ {}",
                                    style.language.tr_with(
                                        "poll_bubble.correct_answer",
                                        &[("options", &answer)]
                                    )
                                )
                            })),
                    )
                },
            )
            .children(
                self.error
                    .map(|error| status_line(palette).text_color(palette.danger).child(error)),
            )
            .children(self.participants.filter(|_| total > 1).map(|count| {
                status_line(palette).child(
                    style
                        .language
                        .tr_with("poll_bubble.participants", &[("count", &count.to_string())]),
                )
            }))
            .children(
                self.add_option
                    .filter(|_| style.can_vote)
                    .map(|add_option| add_option.render(self.id, can_vote, style)),
            )
    }
}

impl PollOption {
    /// Рисует отметку, текст, числа и полосу, если вопрос показывает результаты.
    fn render(self, question: &SharedString, can_vote: bool, style: &PollStyle) -> Stateful<Div> {
        let palette = style.palette;
        let correct = self.correctness == Some(true);
        let highlighted = correct || (self.selected && self.correctness != Some(false));
        let color = if correct {
            palette.presence_online
        } else if highlighted {
            palette.accent
        } else {
            palette.text
        };
        let can_vote = can_vote && self.enabled;
        // Текст и счётчик переносятся в одном блоке, поэтому длинный ответ
        // не отнимает ширину у отметки и аватаров.
        let text = match &self.result {
            Some(result) => format!(
                "{}\u{2002}{}%\u{a0}·\u{a0}{}",
                self.text, result.percent, result.votes
            )
            .into(),
            None => self.text,
        };
        let choice = div()
            .id("poll-choice")
            .flex_1()
            .min_w_0()
            .min_h(px(28.))
            .flex()
            .items_center()
            .gap_x(px(8.))
            .text_color(color)
            .text_size(px(14.))
            .line_height(px(17.))
            .when(can_vote, |choice| choice.hover(|style| style.underline()))
            .child(indicator(
                self.selected || correct,
                if correct { color } else { palette.accent },
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(text)
                    .when(self.result.is_some(), |label| {
                        label.debug_selector(|| "poll-answer-with-result".into())
                    }),
            );
        let choice = poll_control(
            choice,
            can_vote,
            PollAction::Vote {
                question: question.clone(),
                option: self.id.clone(),
                selected: !self.selected,
            },
            style,
        )
        .debug_selector(|| "poll-choice".into());
        let visible_voters = self.voters.len().min(MAX_VOTER_AVATARS);
        let voters =
            (style.show_voters && self.result.is_some() && visible_voters > 0).then(|| {
                // Полоса аватаров занимает место в строке до расчёта ширины текста.
                let width = VOTER_AVATAR_SIZE
                    + (visible_voters - 1) as f32 * (VOTER_AVATAR_SIZE - VOTER_AVATAR_OVERLAP);
                let avatars = div()
                    .id("poll-voters")
                    .w(px(width + 2.))
                    .flex()
                    .flex_none()
                    .items_center()
                    .children(
                        self.voters
                            .into_iter()
                            .take(visible_voters)
                            .enumerate()
                            .map(|(index, avatar)| {
                                div()
                                    .size(px(VOTER_AVATAR_SIZE))
                                    .flex_none()
                                    .rounded_full()
                                    .border_1()
                                    .border_color(palette.surface)
                                    .when(index > 0, |item| item.ml(px(-VOTER_AVATAR_OVERLAP)))
                                    .child(avatar.render_at_with_background(
                                        palette,
                                        px(VOTER_AVATAR_SIZE - 2.),
                                        None,
                                    ))
                            }),
                    );
                poll_control(
                    avatars,
                    true,
                    PollAction::Voters {
                        question: question.clone(),
                        option: self.id.clone(),
                    },
                    style,
                )
                .debug_selector(|| "poll-voters".into())
            });
        div()
            .id(self.id)
            .min_w_0()
            .py(px(6.))
            .child(
                div()
                    .flex()
                    .w_full()
                    .min_w_0()
                    .items_center()
                    .gap(px(8.))
                    .child(choice)
                    .children(voters),
            )
            .child(
                div()
                    .mt(px(6.))
                    .h(px(2.))
                    .min_w_0()
                    .children(self.result.map(|result| {
                        div()
                            .h_full()
                            .w(relative(f32::from(result.percent.min(100)) / 100.))
                            .rounded_full()
                            .bg(if highlighted {
                                palette.accent
                            } else {
                                palette.text_secondary
                            })
                            .debug_selector(|| "poll-result-bar".into())
                    })),
            )
    }
}

#[cfg(all(test, feature = "gpui-test"))]
mod tests;

impl PollAddOption {
    /// Показывает пустую, заполненную, ожидающую или ошибочную строку нового ответа.
    fn render(self, question: SharedString, can_add: bool, style: &PollStyle) -> Div {
        let palette = style.palette;
        let enabled = can_add && !self.pending;
        let (text, color) = if self.text.is_empty() {
            (
                style
                    .language
                    .tr("poll_bubble.add_option_placeholder")
                    .into(),
                palette.text_secondary,
            )
        } else {
            (self.text, palette.text)
        };
        let row = div()
            .id("poll-add-option")
            .flex()
            .min_w_0()
            .min_h(px(36.))
            .pt(px(9.))
            .pb(px(10.))
            .gap(px(8.))
            .text_size(px(14.))
            .line_height(px(17.))
            .when(!enabled, |row| row.opacity(0.5))
            .child(indicator(false, palette.text_secondary))
            .child(div().flex_1().min_w_0().text_color(color).child(text));
        div()
            .mt(px(6.))
            .child(poll_control(
                row,
                enabled,
                PollAction::AddOption { question },
                style,
            ))
            .children(self.error.then(|| {
                div()
                    .text_color(palette.danger)
                    .child(style.language.tr("poll_bubble.add_error"))
            }))
    }
}

/// Рисует общий круг ответа. Цвет верного ответа соответствует зелёному индикатору Workspace.
fn indicator(checked: bool, color: Hsla) -> Div {
    div()
        .size(px(16.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .when(checked, |circle| {
            circle
                .bg(color)
                .child(Icon::Check.element(px(8.), rgb(0xffffff).into()))
        })
        .when(!checked, |circle| circle.border_1().border_color(color))
}

/// Добавляет нажатие мышью, фокус и клавиатуру только для доступного действия.
fn poll_control(
    element: Stateful<Div>,
    enabled: bool,
    action: PollAction,
    style: &PollStyle,
) -> Stateful<Div> {
    if !enabled {
        return element;
    }
    let on_action = style.on_action.clone();
    activate(
        element
            .rounded(px(4.))
            .border_1()
            .border_color(transparent_black()),
        None,
        style.palette,
        move |window, cx| {
            cx.stop_propagation();
            on_action(action.clone(), window, cx);
        },
    )
}

/// Готовит второстепенную строку с одинаковым отступом от вариантов.
fn status_line(palette: Palette) -> Div {
    div()
        .mt(px(6.))
        .min_w_0()
        .text_color(palette.text_secondary)
}
