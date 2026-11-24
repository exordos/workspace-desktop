//! Применяет общий выбор анимаций к переходам GPUI и ручным эффектам.

use gpui::{Animation, AnimationExt, AnyElement, ElementId, IntoElement};

/// Определяет, можно ли проигрывать отдельный эффект в текущем режиме.
#[derive(Clone, Copy)]
pub enum Motion {
    Preference(bool), // Эффект следует настройке локального профиля.
    Always,           // Эффект играет в любом режиме по явному решению компонента.
}

impl Motion {
    /// Учитывает текущее значение общей настройки для обычного эффекта.
    pub fn from_preference(animations_enabled: bool) -> Self {
        Self::Preference(animations_enabled)
    }

    /// Проверяет, может ли эффект создать анимацию или запросить следующий кадр.
    pub fn allows(self) -> bool {
        match self {
            Self::Preference(enabled) => enabled,
            Self::Always => true,
        }
    }

    /// Запускает эффект по стабильному ключу или сразу отрисовывает статичный кадр.
    /// `static_progress` задаёт итог перехода или читаемое состояние цикла.
    /// Одноразовый переход передаётся сюда, пока активно действие, которое его запустило.
    /// Компонент завершает это действие при выключении, чтобы старый переход не запустился снова.
    pub fn animate<E: IntoElement + 'static>(
        self,
        element: E,
        id: impl Into<ElementId>,
        static_progress: f32,
        animation: impl FnOnce() -> Animation,
        render: impl Fn(E, f32) -> E + 'static,
    ) -> AnyElement {
        if self.allows() {
            element
                .with_animation(id, animation(), render)
                .into_any_element()
        } else {
            // GPUI удалит состояние анимации, которую не увидит в новом кадре.
            render(element, static_progress).into_any_element()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc, time::Duration};

    use gpui::{AnimationElement, Div, div};

    use super::*;

    /// Выключенный эффект сразу строит статичный кадр, даже если его фабрика недоступна.
    #[test]
    fn disabled_motion_renders_static_progress_without_creating_animation() {
        let progress = Rc::new(Cell::new(None));
        let captured = progress.clone();
        let mut element = Motion::from_preference(false).animate(
            div(),
            "disabled-motion",
            0.75,
            || panic!("выключенный эффект не должен создавать Animation"),
            move |element, progress| {
                captured.set(Some(progress));
                element
            },
        );

        assert_eq!(progress.get(), Some(0.75));
        assert!(element.downcast_mut::<Div>().is_some());
    }

    /// Включённая настройка и явно заданное исключение создают анимацию для GPUI.
    #[test]
    fn enabled_motion_and_explicit_exceptions_create_animation() {
        for motion in [Motion::from_preference(true), Motion::Always] {
            let created = Cell::new(false);
            let mut element = motion.animate(
                div(),
                "enabled-motion",
                1.,
                || {
                    created.set(true);
                    Animation::new(Duration::from_secs(1))
                },
                |_, _| panic!("GPUI должен отрисовать анимацию в окне"),
            );

            assert!(created.get());
            assert!(element.downcast_mut::<AnimationElement<Div>>().is_some());
        }
    }
}
