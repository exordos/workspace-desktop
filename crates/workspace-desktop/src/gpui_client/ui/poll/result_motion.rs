//! Хранит ширины полосок видимого вопроса и рисует все варианты с одним прогрессом.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use gpui::{App, IntoElement, RenderOnce, SharedString, Window};

use super::{PollOption, PollStyle, answer_options};

const DURATION: Duration = Duration::from_millis(200);

type Widths = HashMap<SharedString, f32>;

/// Список вариантов одного вопроса, который GPUI раскладывает после получения прогресса.
#[derive(IntoElement)]
pub(super) struct AnimatedOptions {
    question: SharedString,   // Ключ вопроса для действий над ответами.
    options: Vec<PollOption>, // Актуальные варианты и разрешённые для показа результаты.
    can_vote: bool,           // Вопрос сейчас принимает ответы.
    style: PollStyle,         // Палитра, настройка движения и обработчик действий.
}

impl AnimatedOptions {
    /// Готовит варианты для отрисовки внутри элемента с ID вопроса.
    pub(super) fn new(
        question: SharedString,
        options: Vec<PollOption>,
        can_vote: bool,
        style: &PollStyle,
    ) -> Self {
        Self {
            question,
            options,
            can_vote,
            style: style.clone(),
        }
    }
}

impl RenderOnce for AnimatedOptions {
    /// Берёт предыдущий кадр вопроса и запрашивает один кадр, пока движутся полоски.
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        if !self.style.motion.allows() {
            // Список сохраняет путь фокуса. GPUI удаляет состояние самого перехода.
            return answer_options(
                &self.question,
                self.options,
                self.can_vote,
                &self.style,
                PollOption::target_width,
            );
        }
        // GPUI добавляет ключ к пути элементов панели, опроса и вопроса.
        window.with_global_id("poll-result-motion".into(), |id, window| {
            window.with_element_state(id, |previous: Option<ResultMotion>, window| {
                let mut state = previous.unwrap_or_else(|| ResultMotion::new(&self.options));
                let now = cx.background_executor().now();
                let progress = state.update(&self.options, now);
                if state.transition.is_some() {
                    window.request_animation_frame();
                }
                let options = answer_options(
                    &self.question,
                    self.options,
                    self.can_vote,
                    &self.style,
                    |option| state.width(&option.id, progress),
                );
                (options, state)
            })
        })
    }
}

/// Помнит целевые ширины и переход между кадрами видимого вопроса.
struct ResultMotion {
    target: Widths, // Ширины актуальных вариантов от 0 до 1, скрытая полоса занимает 0.
    transition: Option<Transition>, // None, когда все полоски показывают итоговую ширину.
}

/// Общий момент старта и ширины полосок, которые реально меняются.
struct Transition {
    from: Widths,     // Видимые ширины от 0 до 1 в начале перехода.
    started: Instant, // Момент получения новых результатов.
}

impl ResultMotion {
    /// Показывает уже доступную статистику сразу при первом появлении опроса.
    fn new(options: &[PollOption]) -> Self {
        Self {
            target: Self::widths(options),
            transition: None,
        }
    }

    /// Читает ширины актуального снимка по стабильным ключам вариантов.
    fn widths(options: &[PollOption]) -> Widths {
        options
            .iter()
            .map(|option| (option.id.clone(), option.target_width()))
            .collect()
    }

    /// Обновляет цели, сохраняет непрерывность при переголосовании и возвращает прогресс кадра.
    fn update(&mut self, options: &[PollOption], now: Instant) -> f32 {
        let changed = self.target.len() != options.len()
            || options
                .iter()
                .any(|option| self.target.get(&option.id).copied() != Some(option.target_width()));
        let progress = self.progress(now);
        if changed {
            let from: Widths = options
                .iter()
                .filter(|option| option.result.is_some())
                .filter_map(|option| {
                    let shown = self.width(&option.id, progress);
                    (shown != option.target_width()).then(|| (option.id.clone(), shown))
                })
                .collect();
            self.target = Self::widths(options);
            self.transition = (!from.is_empty()).then_some(Transition { from, started: now });
            return if self.transition.is_some() { 0. } else { 1. };
        }
        if progress >= 1. {
            self.transition = None;
        }
        progress
    }

    /// Возвращает общую долю перехода с мягким замедлением к его концу.
    fn progress(&self, now: Instant) -> f32 {
        self.transition.as_ref().map_or(1., |transition| {
            let elapsed = now.saturating_duration_since(transition.started);
            let fraction = (elapsed.as_secs_f32() / DURATION.as_secs_f32()).min(1.);
            1. - (1. - fraction).powi(2)
        })
    }

    /// Возвращает ширину отдельной полоски от 0 до 1 на общей доле перехода.
    fn width(&self, option: &SharedString, progress: f32) -> f32 {
        let target = self.target.get(option).copied().unwrap_or_default();
        let from = self
            .transition
            .as_ref()
            .and_then(|transition| transition.from.get(option));
        from.map_or(target, |from| from + (target - from) * progress)
    }
}
