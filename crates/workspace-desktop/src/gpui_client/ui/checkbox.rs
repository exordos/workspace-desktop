//! Рисует отметку чекбокса и анимирует смену выбора. Клики и фокус обрабатывает строка.

use std::time::{Duration, Instant};

use gpui::{
    App, Div, ElementId, Hsla, IntoElement, Pixels, RenderOnce, Window, div, ease_in_out,
    prelude::*, px, rgb,
};

use super::{Icon, Palette, animation::Motion};

const TRANSITION_TIME: Duration = Duration::from_millis(120);

/// Отметка управляемого чекбокса размером 16 × 16 px.
#[derive(IntoElement)]
pub struct Checkbox {
    id: ElementId,          // Стабильный ключ отметки внутри строки.
    checked: bool,          // Выбор, который передаёт вызывающая строка.
    appearance: Appearance, // Форма и цвета отметки.
    motion: Motion,         // Разрешено ли анимировать смену выбора.
}

/// Визуальные свойства отметки в состоянии выбора и без него.
struct Appearance {
    radius: Pixels,     // Радиус скругления границы.
    check_size: Pixels, // Длина стороны иконки галочки.
    border: Hsla,       // Цвет границы пустой отметки.
    fill: Hsla,         // Цвет заливки и границы выбранной отметки.
    check: Hsla,        // Цвет галочки на выбранной отметке.
}

impl Checkbox {
    /// Создаёт круглую отметку для строки ответа, с заданным цветом выбора.
    pub fn circle(id: impl Into<ElementId>, checked: bool, color: Hsla, motion: Motion) -> Self {
        Self {
            id: id.into(),
            checked,
            appearance: Appearance {
                radius: px(8.),
                check_size: px(8.),
                border: color,
                fill: color,
                check: rgb(0xffffff).into(),
            },
            motion,
        }
    }

    /// Создаёт квадратную отметку для строки настройки, с цветами темы.
    pub fn square(
        id: impl Into<ElementId>,
        checked: bool,
        palette: Palette,
        motion: Motion,
    ) -> Self {
        Self {
            id: id.into(),
            checked,
            appearance: Appearance {
                radius: px(3.),
                check_size: px(12.),
                border: palette.icon,
                fill: palette.accent,
                check: palette.on_accent,
            },
            motion,
        }
    }

    /// Строит кадр с нужной прозрачностью заливки и галочки внутри прежних границ.
    fn indicator(&self, progress: f32) -> Div {
        let appearance = &self.appearance;
        div()
            .size(px(16.))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(appearance.radius)
            .border_1()
            .border_color(appearance.border.blend(appearance.fill.opacity(progress)))
            .when(progress > 0., |indicator| {
                indicator.bg(appearance.fill.opacity(progress)).child(
                    Icon::Check
                        .element(appearance.check_size, appearance.check)
                        .opacity(progress),
                )
            })
    }
}

/// Хранит начало текущего перехода между кадрами одной отметки.
struct Transition {
    checked: bool,            // Целевое состояние отметки.
    from: f32,                // Прозрачность в начале перехода, от 0 до 1.
    started: Option<Instant>, // Момент начала движения или None для статичного кадра.
}

impl Transition {
    /// Начинает сразу с текущего состояния при первом появлении отметки.
    fn new(checked: bool) -> Self {
        Self {
            checked,
            from: f32::from(checked),
            started: None,
        }
    }

    /// Вычисляет прозрачность и начинает новый переход с текущего кадра при смене выбора.
    fn update(&mut self, checked: bool, now: Instant) -> f32 {
        let target = f32::from(self.checked);
        let progress = self.started.map_or(target, |started| {
            let elapsed = now.saturating_duration_since(started);
            let delta = (elapsed.as_secs_f32() / TRANSITION_TIME.as_secs_f32()).min(1.);
            self.from + (target - self.from) * ease_in_out(delta)
        });
        self.started = if checked != self.checked {
            self.checked = checked;
            self.from = progress;
            (progress != f32::from(checked)).then_some(now)
        } else {
            self.started.filter(|_| progress != target)
        };
        progress
    }
}

impl RenderOnce for Checkbox {
    /// Подготавливает текущий кадр отметки. При выключенной анимации сразу строит итог.
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let progress = if self.motion.allows() {
            window.with_global_id(self.id.clone(), |id, window| {
                window.with_element_state(id, |state, window| {
                    let mut transition = state.unwrap_or_else(|| Transition::new(self.checked));
                    let progress = transition.update(self.checked, cx.background_executor().now());
                    if transition.started.is_some() {
                        window.request_animation_frame();
                    }
                    (progress, transition)
                })
            })
        } else {
            // GPUI отбросит состояние, за которым мы не обращаемся в новом кадре.
            f32::from(self.checked)
        };
        self.indicator(progress)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Проверяет статичный первый кадр и остановку при достижении выбранного состояния.
    #[test]
    fn transition_starts_only_after_selection_changes() {
        let now = Instant::now();
        let mut state = Transition::new(true);
        assert_eq!(state.update(true, now), 1.);
        assert!(state.started.is_none());
        assert_eq!(state.update(false, now), 1.);
        assert_eq!(state.update(false, now + TRANSITION_TIME / 2), 0.5);
        assert_eq!(state.update(false, now + TRANSITION_TIME), 0.);
        assert!(state.started.is_none());
    }

    /// Проверяет, что быстрый повторный выбор меняет направление без скачка.
    #[test]
    fn transition_reverses_from_current_frame() {
        let now = Instant::now();
        let mut state = Transition::new(false);
        assert_eq!(state.update(true, now), 0.);
        let reverse = now + TRANSITION_TIME / 2;
        let progress = state.update(false, reverse);
        assert_eq!(progress, 0.5);
        assert_eq!(state.update(false, reverse), progress);
        assert_eq!(state.update(false, reverse + TRANSITION_TIME / 2), 0.25);
        assert_eq!(state.update(false, reverse + TRANSITION_TIME), 0.);
        assert!(state.started.is_none());
    }
}
