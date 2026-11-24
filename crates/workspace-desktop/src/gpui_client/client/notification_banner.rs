//! Показывает запрос разрешения поверх чатов. Хранит сворачивание, наведение и фокус,
//! а системные действия и состояние разрешения получает от оболочки.

use std::time::{Duration, Instant};

use gpui::{
    App, Context, Div, FocusHandle, IntoElement, KeyDownEvent, Render, Subscription, Window, div,
    prelude::*, px,
};

use crate::{
    gpui_client::{
        preferences::Preferences,
        ui::{Button, ButtonAppearance, Palette, TextRole, label, popup::tooltip},
    },
    platform::notifications::NotificationPermission,
};

const STRIP_HEIGHT: f32 = 8.;
const CONTENT_HEIGHT: f32 = 94.;
const SLIDE_TIME: Duration = Duration::from_millis(150);

/// Источник системного состояния и действия, которые запрашивает планка.
#[derive(Clone, Copy)]
pub(super) struct NotificationBannerActions {
    pub status: fn(&App) -> (Option<NotificationPermission>, bool), // Статус и ожидание ответа ОС.
    pub request: fn(&mut App),                                      // Попросить разрешение macOS.
    pub open_settings: fn(&App) -> bool, // Открыть настройки или сообщить о сбое запуска.
}

/// Режим показа планки в течение запуска приложения.
#[derive(Clone, Copy, PartialEq)]
enum BannerMode {
    Pinned,    // Планка раскрыта до явного сворачивания.
    Collapsed, // Видна только полоска.
    Preview,   // Планка раскрыта наведением и держится, пока внутри указатель или фокус.
}

/// Управляет высотой, фокусом и действиями планки одного окна.
pub(super) struct NotificationBanner {
    actions: NotificationBannerActions, // Состояние и действия владельца интеграции с ОС.
    mode: BannerMode,                   // Как долго планка остаётся раскрытой.
    hovered: bool,                      // Указатель находится над текущей видимой частью.
    hover_blocked: bool,                // После сворачивания ждём выхода указателя.
    slide_from: f32,                    // Доля раскрытия в начале текущего перехода.
    slide_started: Option<Instant>,     // Момент начала анимации, если планка уже переключалась.
    settings_failed: bool,              // Вместо описания показать ручной путь в настройки.
    group: FocusHandle,                 // Родитель фокуса полоски и кнопок.
    strip: FocusHandle,                 // Сюда возвращаем фокус после сворачивания.
    primary: FocusHandle,               // Кнопка запроса или перехода в настройки.
    later: FocusHandle,                 // Кнопка «Не сейчас».
    collapse: FocusHandle,              // Кнопка со стрелкой сворачивания.
    return_focus: FocusHandle,          // Фокус рабочего окна после исчезновения планки.
    focus_out: Option<Subscription>,    // Закрывает предпросмотр после ухода фокуса.
}

impl NotificationBanner {
    /// Создаёт планку. До получения статуса ОС она скрыта.
    pub(super) fn new(
        actions: NotificationBannerActions,
        return_focus: FocusHandle,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            actions,
            mode: BannerMode::Pinned,
            hovered: false,
            hover_blocked: false,
            slide_from: 0.,
            slide_started: None,
            settings_failed: false,
            group: cx.focus_handle(),
            strip: cx.focus_handle(),
            primary: cx.focus_handle(),
            later: cx.focus_handle(),
            collapse: cx.focus_handle(),
            return_focus,
            focus_out: None,
        }
    }

    /// Возвращает направление анимации для режима показа.
    fn target(mode: BannerMode) -> f32 {
        if mode == BannerMode::Collapsed {
            0.
        } else {
            1.
        }
    }

    /// Вычисляет долю открытия, сохраняя положение при развороте анимации.
    fn progress(&self, now: Instant) -> f32 {
        let Some(started) = self.slide_started else {
            return self.slide_from;
        };
        let elapsed = now.saturating_duration_since(started);
        let fraction = (elapsed.as_secs_f32() / SLIDE_TIME.as_secs_f32()).min(1.);
        let eased = gpui::ease_in_out(fraction);
        self.slide_from + (Self::target(self.mode) - self.slide_from) * eased
    }

    /// Меняет режим и запускает движение из текущего положения.
    fn set_mode(&mut self, mode: BannerMode, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        if Self::target(self.mode) != Self::target(mode) {
            let now = cx.background_executor().now();
            self.slide_from = self.progress(now);
            self.slide_started = Some(now);
        }
        self.mode = mode;
        cx.notify();
    }

    /// Обрабатывает новое наведение и закрывает предпросмотр при выходе.
    fn hover(&mut self, hovered: bool, window: &Window, cx: &mut Context<Self>) {
        self.hovered = hovered;
        if hovered && !self.hover_blocked && self.mode == BannerMode::Collapsed {
            self.set_mode(BannerMode::Preview, cx);
        }
        if !hovered {
            self.hover_blocked = false;
            let controls_focused =
                self.group.contains_focused(window, cx) && !self.strip.is_focused(window);
            if self.mode == BannerMode::Preview && !controls_focused {
                self.set_mode(BannerMode::Collapsed, cx);
            }
        }
    }

    /// Сворачивает планку без вызова системного API.
    fn collapse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.hover_blocked = self.hovered;
        self.set_mode(BannerMode::Collapsed, cx);
        window.focus(&self.strip);
    }

    /// Рисует только доступное для текущих настроек действие и передаёт нажатие оболочке.
    fn content(
        &self,
        permission: NotificationPermission,
        requesting: bool,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> Div {
        let open = self.mode != BannerMode::Collapsed;
        let needs_settings = permission == NotificationPermission::NeedsSettings;
        let language = Preferences::read(cx).language;
        let (title, description, action) = if needs_settings {
            (
                "desktop.notifications.disabledTitle",
                if self.settings_failed {
                    "desktop.notifications.settingsPath"
                } else {
                    "desktop.notifications.settingsHint"
                },
                "desktop.notifications.openSettings",
            )
        } else {
            (
                "notifications.permissionBannerTitle",
                "desktop.notifications.permissionBody",
                if requesting {
                    "desktop.notifications.waitingForSystem"
                } else {
                    "notifications.permissionBannerEnable"
                },
            )
        };

        let mut primary = Button::new("notification-permission-primary", language.tr(action));
        primary.appearance = ButtonAppearance::Primary;
        primary.disabled = requesting || !open;
        let mut later = Button::new(
            "notification-permission-later",
            language.tr("notifications.permissionBannerDismiss"),
        );
        later.disabled = !open;
        let mut collapse = Button::new("notification-permission-collapse", "⌃");
        collapse.appearance = ButtonAppearance::Ghost;
        collapse.disabled = !open;
        let view = cx.entity().downgrade();
        let actions = self.actions;
        let dismiss = move |window: &mut Window, app: &mut App| {
            let _ = view.update(app, |banner, cx| banner.collapse(window, cx));
        };
        let view = cx.entity().downgrade();

        div()
            .absolute()
            .top(px(STRIP_HEIGHT))
            .left_0()
            .right_0()
            .h(px(CONTENT_HEIGHT))
            .px_4()
            .py_2()
            .flex()
            .flex_col()
            .justify_center()
            .gap_2()
            .bg(palette.surface)
            .border_b_1()
            .border_color(palette.border)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(label(palette, TextRole::Heading, language.tr(title)))
                    .child(
                        label(palette, TextRole::Body, language.tr(description))
                            .text_color(palette.text_secondary),
                    ),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        primary
                            .render(palette, Some(&self.primary), move |_, app| {
                                if needs_settings {
                                    let opened = (actions.open_settings)(app);
                                    let _ = view.update(app, |banner, cx| {
                                        banner.settings_failed = !opened;
                                        cx.notify();
                                    });
                                } else {
                                    (actions.request)(app);
                                }
                            })
                            .debug_selector(|| "notification-permission-primary".into()),
                    )
                    .child(
                        later
                            .render(palette, Some(&self.later), dismiss.clone())
                            .debug_selector(|| "notification-permission-later".into()),
                    )
                    .child(
                        collapse
                            .render(palette, Some(&self.collapse), dismiss)
                            .tooltip(move |_, app| {
                                tooltip(
                                    palette,
                                    Preferences::read(app)
                                        .language
                                        .tr("desktop.notifications.collapse")
                                        .into(),
                                    app,
                                )
                            }),
                    ),
            )
    }

    /// Рисует абсолютный слой, чтобы изменение его высоты не сдвигало список чатов.
    fn overlay(
        &mut self,
        permission: NotificationPermission,
        requesting: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let now = cx.background_executor().now();
        self.slide_started.get_or_insert(now);
        let progress = self.progress(now);
        if now.saturating_duration_since(self.slide_started.unwrap()) < SLIDE_TIME {
            window.request_animation_frame();
        }
        if self.focus_out.is_none() {
            self.focus_out = Some(cx.on_focus_out(&self.group, window, |banner, _, _, cx| {
                if banner.mode == BannerMode::Preview && !banner.hovered {
                    banner.set_mode(BannerMode::Collapsed, cx);
                }
            }));
        }
        let palette = Palette::for_window(window, cx);
        let strip_view = cx.entity().downgrade();
        let strip = div()
            .id("notification-permission-strip")
            .debug_selector(|| "notification-permission-strip".into())
            .h(px(STRIP_HEIGHT))
            .w_full()
            .bg(palette.accent)
            .when(self.mode != BannerMode::Pinned, |strip| {
                strip
                    .track_focus(&self.strip.clone().tab_stop(true))
                    .tab_stop(true)
                    .cursor_pointer()
                    .when(palette.keyboard_focus_visible(), |strip| {
                        strip.focus(|style| style.bg(palette.focus))
                    })
                    .on_click(move |_, window, app| {
                        let _ = strip_view.update(app, |banner, cx| {
                            banner.set_mode(BannerMode::Pinned, cx);
                            if !requesting {
                                window.focus(&banner.primary);
                            } else {
                                window.focus(&banner.later);
                            }
                        });
                    })
                    .tooltip(move |_, app| {
                        tooltip(
                            palette,
                            Preferences::read(app)
                                .language
                                .tr("desktop.notifications.expand")
                                .into(),
                            app,
                        )
                    })
            });

        div()
            .id("notification-permission-banner")
            .debug_selector(|| "notification-permission-banner".into())
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .h(px(STRIP_HEIGHT + CONTENT_HEIGHT * progress))
            .overflow_hidden()
            .occlude()
            .track_focus(&self.group)
            .on_hover(cx.listener(|banner, hovered, window, cx| banner.hover(*hovered, window, cx)))
            .on_key_down(cx.listener(|banner, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" && banner.mode != BannerMode::Collapsed {
                    banner.collapse(window, cx);
                    cx.stop_propagation();
                }
            }))
            .children(
                (progress > 0. || self.mode != BannerMode::Collapsed).then(|| {
                    self.content(permission, requesting, palette, cx)
                        .opacity(progress)
                }),
            )
            .child(strip)
    }
}

impl Render for NotificationBanner {
    /// Читает текущий статус окна и убирает планку, когда macOS разрешила баннеры.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (permission, requesting) = (self.actions.status)(cx);
        let visible = permission.filter(|value| *value != NotificationPermission::Enabled);
        if visible.is_none()
            && (self.group.contains_focused(window, cx) || self.primary.is_focused(window))
        {
            window.focus(&self.return_focus);
        } else if requesting && self.primary.is_focused(window) {
            // Недоступная кнопка пропадает из обхода фокуса, пока открыт системный диалог.
            window.focus(&self.later);
        }
        div()
            .absolute()
            .inset_0()
            .children(visible.map(|permission| self.overlay(permission, requesting, window, cx)))
    }
}
