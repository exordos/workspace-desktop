//! Рисует быстрые команды у границ сообщения и раскрывает список реакций под указателем.
//! Хранит наведение, фокус и переход капсулы только для видимых пузырей.

use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{
    App, Bounds, DispatchPhase, Edges, Element, ElementId, FocusHandle, GlobalElementId, Hitbox,
    HitboxBehavior, InspectorElementId, LayoutId, MouseExitEvent, MouseMoveEvent, Pixels, Point,
    Position, ScrollHandle, SharedString, Size, Style, Task, Window, div, point, prelude::*, px,
};
use uuid::Uuid;
use workspace_core::messenger::quick_reactions::reaction_glyph;

use super::reactions::{QuickReaction, ReactionControls};
use crate::gpui_client::{
    preferences::{Language, Preferences},
    ui::{
        Button, ButtonAppearance, Icon, Palette, TextRole, animation::Motion, label, popup::tooltip,
    },
};

const OPEN_DELAY: Duration = Duration::from_millis(150);
const CLOSE_DELAY: Duration = Duration::from_millis(200);
const CAPSULE_TRANSITION: Duration = Duration::from_millis(140);
const PILL_HEIGHT: Pixels = px(24.);
const COLLAPSED: Pixels = px(24.);
const CAPSULE: Pixels = px(40.);
const EMOJI_BUTTON: Pixels = px(36.);
const MAX_CAPSULE_HEIGHT: Pixels = px(176.);
const EDGE: Pixels = px(4.);

/// Обработчик уже проверенного действия текущего разговора.
type Activate = Rc<dyn Fn(&mut Window, &mut App)>;

/// Обработчик выбора эмодзи в панели быстрых реакций.
type SelectEmoji = Rc<dyn Fn(&str, &mut App)>;

/// Команда с текстовой подписью на верхней стороне пузыря.
pub(super) struct QuickCommand {
    name: &'static str,  // Устойчивое имя элемента и селектора GPUI.
    label: SharedString, // Подпись на языке разговора.
    icon: Option<Icon>,  // Дополнительная пиктограмма слева.
    always: bool,        // Запрос ключа виден и без указателя.
    disabled: bool,      // Операция уже запущена для этого сообщения.
    activate: Activate,  // Команда экрана, перепроверяющая исходную строку.
}

impl QuickCommand {
    /// Создаёт команду, которая появляется при наведении на пузырь.
    pub(super) fn on_hover(
        name: &'static str,
        label: &'static str,
        activate: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            name,
            label: label.into(),
            icon: None,
            always: false,
            disabled: false,
            activate: Rc::new(activate),
        }
    }

    /// Оставляет команду видимой у закрытого сообщения.
    pub(super) fn always(mut self, icon: Icon, disabled: bool) -> Self {
        self.always = true;
        self.icon = Some(icon);
        self.disabled = disabled;
        self
    }
}

/// Привязывает быстрые команды и список реакций к текущему пузырю, сохраняя размер строки.
pub(super) struct QuickActions {
    pub id: Uuid,                          // Публичный UUID серверной строки.
    pub own: bool,                         // С какой стороны оставить свободное место.
    pub palette: Palette,                  // Оформление панели поверх пузыря.
    pub commands: Vec<QuickCommand>,       // Команды в порядке отображения верхней панели.
    pub reactions: Option<QuickReactions>, // Варианты реакции для этого пузыря.
}

/// Снимок быстрых реакций одного сообщения и обработчик выбора.
pub(super) struct QuickReactions {
    choices: Rc<[QuickReaction]>, // Эмодзи и их состояние в порядке использования.
    activate: SelectEmoji,        // Передаёт выбранный эмодзи общему обработчику.
}

/// Сохраняет границы сообщения и области истории для обеих частей панели.
#[derive(Clone, Copy)]
struct Placement {
    bubble: Bounds<Pixels>,   // Координаты самой подложки сообщения.
    viewport: Bounds<Pixels>, // Текущая видимая часть истории, которую можно занимать.
}

impl QuickReactions {
    /// Готовит колонку при наличии хотя бы одной доступной по типу сообщения реакции.
    pub(super) fn new(
        choices: Vec<QuickReaction>,
        activate: impl Fn(&str, &mut App) + 'static,
    ) -> Option<Self> {
        (!choices.is_empty()).then(|| Self {
            choices: choices.into(),
            activate: Rc::new(activate),
        })
    }
}

/// Участок, по которому можно перевести указатель, сохранив панель.
#[derive(Clone, Copy)]
enum Area {
    Bubble = 0,   // Сам пузырь.
    Commands = 1, // Верхняя панель.
    Capsule = 2,  // Кнопка или раскрытая колонка эмодзи.
}

/// Начало движения капсулы к текущей цели.
struct CapsuleTransition {
    from: f32,        // Доля раскрытия от 0 до 1 в начале движения.
    started: Instant, // Часы GPUI, которые можно продвинуть в тестовом окне.
}

/// Состояние капсулы в текущем кадре.
#[derive(Clone, Copy)]
struct CapsuleFrame {
    open: bool,    // Дополнительные реакции уже доступны для выбора.
    progress: f32, // Видимая доля раскрытия от 0 до 1.
}

/// Краткое состояние одного видимого сообщения.
pub(super) struct Interaction {
    areas: [bool; 3],                      // Где сейчас находится указатель.
    visible: bool,                         // Сохраняет панель в коротком переходе между участками.
    open: bool,                            // Колонка раскрыта наведением или клавиатурой.
    transition: Option<CapsuleTransition>, // None, когда капсула уже в конечном положении.
    suppressed: bool,                      // Не открывать сразу после выбора или Escape.
    keyboard_open: bool,                   // Колонка раскрыта клавиатурным фокусом.
    panel_timer: Option<Task<()>>,         // Отложенное скрытие всей панели.
    strip_timer: Option<Task<()>>,         // Отложенное раскрытие или закрытие колонки.
    command_focus: FocusHandle,            // Фокус первой верхней команды.
    trigger_focus: FocusHandle, // Одна и та же кнопка для первого эмодзи в обоих состояниях.
    reaction_focus: Vec<FocusHandle>, // Фокус вариантов после первого.
    scroll: ScrollHandle,       // Положение внутри длинной колонки реакций.
}

impl Interaction {
    /// Создаёт состояние для строки при первом появлении в кадре.
    fn new(cx: &App) -> Self {
        Self {
            areas: [false; 3],
            visible: false,
            open: false,
            transition: None,
            suppressed: false,
            keyboard_open: false,
            panel_timer: None,
            strip_timer: None,
            command_focus: cx.focus_handle().tab_stop(true),
            trigger_focus: cx.focus_handle().tab_stop(true),
            reaction_focus: Vec::new(),
            scroll: ScrollHandle::new(),
        }
    }

    /// Мгновенно меняет состояние для клавиатуры, явного выбора или скрытия.
    fn set_open(&mut self, open: bool) {
        self.transition = None;
        if self.open != open {
            self.open = open;
            self.scroll.scroll_to_bottom();
        }
    }

    /// Возвращает видимую долю раскрытия с замедлением к концу перехода.
    fn progress(&self, now: Instant) -> f32 {
        let target = f32::from(self.open);
        self.transition.as_ref().map_or(target, |transition| {
            let elapsed = now.saturating_duration_since(transition.started);
            let time = (elapsed.as_secs_f32() / CAPSULE_TRANSITION.as_secs_f32()).min(1.);
            let eased = 1. - (1. - time).powi(2);
            transition.from + (target - transition.from) * eased
        })
    }

    /// Меняет направление наведения, сохраняя уже показанное положение.
    fn set_hover_open(&mut self, open: bool, cx: &App) {
        if self.open == open || (!open && self.keyboard_open) {
            return;
        }
        if !capsule_motion(cx).allows() {
            self.set_open(open);
            return;
        }
        let now = cx.background_executor().now();
        let from = self.progress(now);
        if open && from == 0. {
            self.scroll.scroll_to_bottom();
        }
        self.open = open;
        self.transition =
            (from != f32::from(open)).then_some(CapsuleTransition { from, started: now });
    }

    /// Строит кадр и планирует следующий только пока движение разрешено и продолжается.
    fn capsule_frame(&mut self, window: &Window, cx: &App) -> CapsuleFrame {
        let mut progress = f32::from(self.open);
        if let Some(transition) = &self.transition {
            let now = cx.background_executor().now();
            if capsule_motion(cx).allows()
                && now.saturating_duration_since(transition.started) < CAPSULE_TRANSITION
            {
                progress = self.progress(now);
                window.request_animation_frame();
            } else {
                self.transition = None;
            }
        }
        CapsuleFrame {
            open: self.open,
            progress,
        }
    }

    /// Показывает элемент с клавиатурным фокусом до ухода пользователя из панели.
    fn keyboard_state(&mut self, palette: Palette, window: &Window) -> (bool, bool) {
        let command = self.command_focus.is_focused(window);
        let trigger = self.trigger_focus.is_focused(window);
        let reaction = self
            .reaction_focus
            .iter()
            .any(|focus| focus.is_focused(window));
        let keyboard_input = palette.keyboard_focus_visible();
        let keyboard = keyboard_input && (trigger || reaction);
        if self.keyboard_open && !keyboard && !self.areas[Area::Capsule as usize] {
            self.set_open(false);
        }
        self.keyboard_open = keyboard;
        if !trigger && !reaction && !self.areas[Area::Capsule as usize] {
            self.suppressed = false;
        }
        (keyboard_input && (command || trigger || reaction), keyboard)
    }
}

/// Читает общий выбор анимаций для перехода капсулы.
fn capsule_motion(cx: &App) -> Motion {
    Motion::from_preference(Preferences::animations_enabled(cx))
}

/// Удерживает панель при переходе из пузыря в вынесенный контрол.
fn change_hover(
    state: &Rc<RefCell<Interaction>>,
    area: Area,
    hovered: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let mut interaction = state.borrow_mut();
    let index = area as usize;
    if interaction.areas[index] == hovered {
        return;
    }
    interaction.areas[index] = hovered;
    interaction.panel_timer = None;
    if interaction.areas.contains(&true) {
        if !interaction.visible {
            interaction.visible = true;
            window.refresh();
        }
    } else {
        let state = Rc::downgrade(state);
        let timer = cx.background_executor().timer(CLOSE_DELAY);
        interaction.panel_timer = Some(window.spawn(cx, async move |cx| {
            timer.await;
            let Some(state) = state.upgrade() else { return };
            let _ = cx.update(|window, cx| {
                let mut state = state.borrow_mut();
                state.visible = false;
                state.set_hover_open(false, cx);
                window.refresh();
            });
        }));
    }
    if index != Area::Capsule as usize {
        return;
    }
    interaction.strip_timer = None;
    if !hovered {
        interaction.suppressed = false;
    }
    if interaction.open == hovered || (hovered && interaction.suppressed) {
        return;
    }
    if hovered
        && interaction.transition.is_some()
        && interaction.progress(cx.background_executor().now()) > 0.
    {
        // Пока капсула сворачивается, повторный вход сразу разворачивает её.
        interaction.set_hover_open(true, cx);
        window.refresh();
        return;
    }
    let state = Rc::downgrade(state);
    let timer = cx
        .background_executor()
        .timer(if hovered { OPEN_DELAY } else { CLOSE_DELAY });
    interaction.strip_timer = Some(window.spawn(cx, async move |cx| {
        timer.await;
        let Some(state) = state.upgrade() else { return };
        let _ = cx.update(|window, cx| {
            state.borrow_mut().set_hover_open(hovered, cx);
            window.refresh();
        });
    }));
}

impl QuickActions {
    /// Выносит текстовую команду за верхнюю кромку пузыря.
    fn commands(
        &self,
        cx: &mut App,
        window: &mut Window,
        state: &Rc<RefCell<Interaction>>,
        placement: Placement,
        visible: bool,
    ) {
        let Placement { bubble, viewport } = placement;
        let id = self.id;
        let palette = self.palette;
        let focus = state.borrow().command_focus.clone();
        let mut toolbar = div()
            .id("bubble-commands")
            .flex()
            .gap_1()
            .max_w((viewport.size.width - EDGE * 2.).max(px(0.)));
        if visible || self.commands.iter().any(|command| command.always) {
            for (index, command) in self.commands.iter().enumerate() {
                if !(visible || command.always) {
                    continue;
                }
                let mut button = Button::new(command.name, command.label.clone());
                button.icon = command.icon;
                button.disabled = command.disabled;
                let name = command.name;
                let activate = command.activate.clone();
                toolbar = toolbar.child(
                    button
                        .render(
                            palette,
                            (index == 0).then_some(&focus),
                            move |window, cx| {
                                cx.stop_propagation();
                                activate(window, cx);
                            },
                        )
                        .min_w_0()
                        .h(PILL_HEIGHT)
                        .rounded_full()
                        .bg(palette.surface)
                        .shadow_md()
                        .border_1()
                        .border_color(palette.border)
                        .debug_selector(move || format!("{name}-{id}")),
                );
            }
            let hover = state.clone();
            toolbar = toolbar
                .block_mouse_except_scroll()
                .on_hover(move |hovered, window, cx| {
                    change_hover(&hover, Area::Commands, *hovered, window, cx);
                });
        } else if self
            .commands
            .first()
            .is_some_and(|command| !command.disabled)
        {
            toolbar = toolbar.child(
                div()
                    .id("command-focus")
                    .track_focus(&focus)
                    .tab_stop(true)
                    .size_0(),
            );
        }
        place(toolbar, window, cx, |size| {
            let x = if self.own {
                bubble.left() + px(12.)
            } else {
                bubble.right() - px(12.) - size.width
            };
            // Длинная подпись запроса не выходит в сайдбар у узкого входящего пузыря.
            point(
                x.clamp(
                    viewport.left(),
                    (viewport.right() - size.width).max(viewport.left()),
                ),
                (bubble.top() - PILL_HEIGHT / 2.).clamp(
                    viewport.top(),
                    (viewport.bottom() - size.height).max(viewport.top()),
                ),
            )
        });
    }

    /// Рисует первый эмодзи в углу и колонку с ограничением по высоте истории.
    fn reactions(
        &self,
        window: &mut Window,
        cx: &mut App,
        state: &Rc<RefCell<Interaction>>,
        placement: Placement,
        visible: bool,
        keyboard: bool,
    ) {
        let Placement { bubble, viewport } = placement;
        let Some(reactions) = &self.reactions else {
            return;
        };
        let id = self.id;
        let choices = reactions.choices.clone();
        let bottom = (bubble.bottom() + EDGE).min(viewport.bottom());
        if bottom < viewport.top() + COLLAPSED {
            return;
        }
        let center = if self.own {
            bubble.left()
        } else {
            bubble.right()
        };
        let mut interaction = state.borrow_mut();
        if keyboard && !interaction.suppressed && choices.len() > 1 {
            interaction.set_open(true);
        } else if choices.len() == 1 || (!visible && interaction.open) {
            interaction.set_open(false);
        }
        let frame = interaction.capsule_frame(window, cx);
        let expanded = frame.open || frame.progress > 0.;
        let size = COLLAPSED + (CAPSULE - COLLAPSED) * frame.progress;
        let focus = interaction.trigger_focus.clone();
        let left = (center - size / 2.).clamp(
            viewport.left(),
            (viewport.right() - size).max(viewport.left()),
        );
        if !visible && !expanded {
            if !choices[0].pending {
                place(
                    div()
                        .id("reaction-focus")
                        .track_focus(&focus)
                        .tab_stop(true)
                        .size_0(),
                    window,
                    cx,
                    |_| point(left, bottom),
                );
            }
            return;
        }
        if expanded {
            interaction
                .reaction_focus
                .resize_with(choices.len() - 1, || cx.focus_handle());
        }
        let scroll = interaction.scroll.clone();
        let focuses: Vec<_> = std::iter::once(focus)
            .chain(interaction.reaction_focus.iter().cloned())
            .collect();
        drop(interaction);
        let width = (viewport.size.width - EDGE * 2.).max(px(0.)).min(size);
        let full_height = (bottom - viewport.top() - EDGE)
            .clamp(COLLAPSED, MAX_CAPSULE_HEIGHT)
            .min(EMOJI_BUTTON * choices.len() as f32 + EDGE);
        let height = COLLAPSED + (full_height - COLLAPSED) * frame.progress;
        let count = if expanded { choices.len() } else { 1 };
        if expanded && scroll.offset().y <= -scroll.max_offset().height {
            // Держим первую кнопку снизу, пока меняется её размер. Скролл пользователя сохраняем.
            scroll.scroll_to_bottom();
        }
        let palette = self.palette;
        let capsule_state = state.clone();
        let menu =
            div()
                .id("quick-reactions-capsule")
                .w(width)
                .h(height)
                .relative()
                .overflow_hidden()
                .rounded_full()
                .border_1()
                .border_color(palette.border)
                .bg(palette.surface)
                .shadow_md()
                .when(frame.open, |view| view.occlude())
                .when(!frame.open, |view| view.block_mouse_except_scroll())
                .on_hover(move |hovered, window, cx| {
                    change_hover(&capsule_state, Area::Capsule, *hovered, window, cx);
                })
                .debug_selector(move || format!("history-quick-reactions-{id}"))
                .child(
                    div()
                        .id("quick-reactions-scroll")
                        .size_full()
                        .flex()
                        .flex_col()
                        .items_center()
                        .when(expanded, |view| {
                            // Обрезаем колонку снизу вверх, сохраняя её внутреннюю область прокрутки.
                            view.absolute()
                                .left_0()
                                .bottom_0()
                                .h(full_height - px(2.))
                                .overflow_y_scroll()
                                .track_scroll(&scroll)
                                .py(px(frame.progress))
                        })
                        .children(choices[..count].iter().enumerate().rev().map(
                            |(index, choice)| {
                                self.reaction_button(
                                    choice,
                                    index,
                                    frame,
                                    &focuses[index],
                                    state,
                                    &reactions.activate,
                                )
                            },
                        ))
                        .on_key_down({
                            let state = state.clone();
                            move |event, window, cx| {
                                if event.keystroke.modifiers.modified() {
                                    return;
                                }
                                let key = event.keystroke.key.as_str();
                                if key == "escape" && frame.open {
                                    close_capsule(&state, true, window);
                                    cx.stop_propagation();
                                } else if matches!(key, "up" | "down" | "home" | "end")
                                    && choices.len() > 1
                                {
                                    focus_reaction(
                                        &mut state.borrow_mut(),
                                        &choices,
                                        key,
                                        frame.open,
                                        window,
                                        cx,
                                    );
                                    cx.stop_propagation();
                                }
                            }
                        }),
                );
        place(menu, window, cx, |_| point(left, bottom - height));
    }

    /// Показывает одну реакцию тем же контролом в свёрнутом и раскрытом состоянии.
    fn reaction_button(
        &self,
        choice: &QuickReaction,
        index: usize,
        frame: CapsuleFrame,
        focus: &FocusHandle,
        state: &Rc<RefCell<Interaction>>,
        activate: &SelectEmoji,
    ) -> impl IntoElement {
        let id = self.id;
        let palette = self.palette;
        let emoji = choice.emoji.clone();
        let caption = choice.label.clone();
        let state = state.clone();
        let activate = activate.clone();
        let mut button = Button::new(index, "");
        button.appearance = ButtonAppearance::Ghost;
        let available = index == 0 || frame.open;
        button.disabled = choice.pending || !available;
        let progress = if index == 0 { frame.progress } else { 1. };
        let compact_side = COLLAPSED - px(2.);
        let side = compact_side + (EMOJI_BUTTON - compact_side) * progress;
        button
            .render_event(palette, Some(focus), move |event, window, cx| {
                close_capsule(&state, event.is_keyboard(), window);
                cx.stop_propagation();
                activate(&emoji, cx);
            })
            .when(available, |button| {
                button
                    .track_focus(&focus.clone().tab_stop(!choice.pending))
                    .tab_stop(!choice.pending)
                    .tooltip(move |_, cx| tooltip(palette, caption.clone(), cx))
            })
            .size(side)
            .min_w_0()
            .flex_none()
            .p_0()
            .rounded_full()
            .when(choice.own, |button| button.bg(palette.accent_soft))
            .when(index > 0, |button| button.opacity(frame.progress))
            .debug_selector(move || format!("history-quick-reaction-{id}-{index}"))
            .child(
                div()
                    .max_w_full()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(px(14. + 8. * progress))
                    .opacity(if choice.pending { 0.45 } else { 1. })
                    .child(choice.glyph.clone()),
            )
    }
}

/// Открывает колонку с клавиатуры или переходит к следующей доступной реакции.
fn focus_reaction(
    state: &mut Interaction,
    choices: &[QuickReaction],
    key: &str,
    open: bool,
    window: &mut Window,
    cx: &App,
) {
    let count = choices.len();
    let current = state
        .reaction_focus
        .iter()
        .position(|focus| focus.is_focused(window))
        .map_or(0, |index| index + 1);
    let enabled = |&index: &usize| !choices[index].pending;
    let target = match (key, open) {
        ("up", true) => (1..=count)
            .map(|offset| (current + count - offset) % count)
            .find(enabled),
        ("down", true) => (1..=count)
            .map(|offset| (current + offset) % count)
            .find(enabled),
        ("up" | "end", _) => (0..count).rev().find(enabled),
        _ => (0..count).find(enabled),
    };
    if let Some(target) = target {
        state.suppressed = false;
        state.set_open(true);
        state
            .reaction_focus
            .resize_with(count - 1, || cx.focus_handle());
        let focus = if target == 0 {
            &state.trigger_focus
        } else {
            &state.reaction_focus[target - 1]
        };
        window.focus(focus);
        state.scroll.scroll_to_item(count - 1 - target);
        window.refresh();
    }
}

/// Готовит отдельные кнопки со счётчиками и отмечает реакции пользователя.
pub(super) fn reaction_chips(
    id: Uuid,
    controls: &ReactionControls,
    palette: Palette,
    locale: Language,
) -> Vec<gpui::AnyElement> {
    controls
        .groups(id)
        .iter()
        .map(|group| {
            let emoji = SharedString::from(group.emoji.clone());
            let glyph = reaction_glyph(&emoji).into_owned();
            let caption =
                SharedString::from(locale.tr_with("a11y.quickReaction", &[("emoji", &glyph)]));
            let mut button = Button::new((ElementId::Uuid(id), emoji.clone()), "");
            button.disabled = controls.read_only || controls.is_pending(id, &emoji);
            let controls = controls.clone();
            let selected = emoji.clone();
            button
                .render(palette, None, move |_, cx| {
                    cx.stop_propagation();
                    controls.toggle(id, &selected, cx);
                })
                .h(px(22.))
                .min_w_0()
                .px(px(6.))
                .gap(px(4.))
                .flex_none()
                .rounded_full()
                .when(group.is_own, |button| {
                    button.bg(palette.accent_soft).border_color(palette.accent)
                })
                .block_mouse_except_scroll()
                .tooltip(move |_, cx| tooltip(palette, caption.clone(), cx))
                .debug_selector(move || format!("history-reaction-chip-{id}-{emoji}"))
                .child(
                    div()
                        .text_size(px(14.))
                        .max_w(px(48.))
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .child(glyph),
                )
                .child(label(palette, TextRole::Caption, group.count.to_string()))
                .into_any_element()
        })
        .collect()
}

/// Накладывает уже измеренный элемент после рисования всех обычных строк истории.
fn place(
    element: impl IntoElement,
    window: &mut Window,
    cx: &mut App,
    position: impl FnOnce(Size<Pixels>) -> Point<Pixels>,
) {
    let mut element = element.into_any_element();
    let size = element.layout_as_root(gpui::AvailableSpace::min_size(), window, cx);
    window.defer_draw(element, position(size), 0);
}

/// Закрывает список и при клавиатурном выборе возвращает фокус первой реакции.
fn close_capsule(state: &Rc<RefCell<Interaction>>, restore_focus: bool, window: &mut Window) {
    let mut interaction = state.borrow_mut();
    interaction.strip_timer = None;
    interaction.suppressed = true;
    interaction.set_open(false);
    if restore_focus {
        window.focus(&interaction.trigger_focus);
    }
    window.refresh();
}

impl Element for QuickActions {
    type RequestLayoutState = ();
    type PrepaintState = Option<(Hitbox, Rc<RefCell<Interaction>>)>;

    /// Держит состояние только пока GPUI строит видимую строку с этим UUID.
    fn id(&self) -> Option<ElementId> {
        Some((ElementId::Uuid(self.id), "message-quick-actions").into())
    }

    /// Это последовательность обычных элементов GPUI без отдельного места в раскладке.
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    /// Получает те же границы, что и поверхность пузыря.
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let style = Style {
            position: Position::Absolute,
            inset: Edges::all(px(0.).into()),
            ..Style::default()
        };
        (window.request_layout(style, None, cx), ())
    }

    /// Вычисляет положение действий внутри видимой области истории.
    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let viewport = window.content_mask().bounds;
        if viewport.intersect(&bounds).is_empty() {
            return None;
        }
        let state = window.with_element_state(
            id.expect("UUID быстрых действий"),
            |state, _| {
                let state = state.unwrap_or_else(|| Rc::new(RefCell::new(Interaction::new(cx))));
                (state.clone(), state)
            },
        );
        let (visible, keyboard) = {
            let mut view = state.borrow_mut();
            let (focused, keyboard) = view.keyboard_state(self.palette, window);
            (view.visible || focused, keyboard)
        };
        let placement = Placement {
            bubble: bounds,
            viewport,
        };
        if !self.commands.is_empty() {
            self.commands(cx, window, &state, placement, visible);
        }
        self.reactions(window, cx, &state, placement, visible, keyboard);
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        Some((hitbox, state))
    }

    /// Следит за указателем только внутри фактически отрисованного пузыря.
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        _: &mut App,
    ) {
        let Some((hitbox, state)) = prepaint.clone() else {
            return;
        };
        // При прокрутке пузырь может уйти из-под неподвижного указателя.
        if state.borrow().areas[Area::Bubble as usize] && !hitbox.is_hovered(window) {
            let state = state.clone();
            let pointer = window.mouse_position();
            window.on_next_frame(move |window, cx| {
                if window.mouse_position() == pointer {
                    change_hover(&state, Area::Bubble, false, window, cx);
                }
            });
        }
        let outside_window = state.clone();
        window.on_mouse_event(move |_: &MouseExitEvent, phase, window, cx| {
            if phase == DispatchPhase::Capture {
                for area in [Area::Bubble, Area::Commands, Area::Capsule] {
                    change_hover(&outside_window, area, false, window, cx);
                }
            }
        });
        window.on_mouse_event(move |_: &MouseMoveEvent, phase, window, cx| {
            if phase == DispatchPhase::Capture {
                change_hover(&state, Area::Bubble, hitbox.is_hovered(window), window, cx);
            }
        });
    }
}

impl IntoElement for QuickActions {
    type Element = Self;

    /// Передаёт наложение GPUI для позиционирования относительно пузыря.
    fn into_element(self) -> Self {
        self
    }
}

#[cfg(all(test, feature = "gpui-test"))]
mod tests {
    use gpui::{Context, Modifiers, Render, ScrollDelta, ScrollWheelEvent, TestAppContext};

    use crate::gpui_client::ui::{ThemeMode, track_focus_input};

    use super::*;

    const ID: Uuid = Uuid::from_u128(42);
    const CAPSULE_BOUNDS: &str = "history-quick-reactions-00000000-0000-0000-0000-00000000002a";
    const SECOND_REACTION: &str = "history-quick-reaction-00000000-0000-0000-0000-00000000002a-1";
    const COMMAND_BOUNDS: &str = "quick-test-command-00000000-0000-0000-0000-00000000002a";

    gpui::actions!(quick_action_tests, [NextQuickAction]);

    /// Окно для проверки реальных рамок и событий без сети и редактора.
    struct Fixture {
        always: bool,                       // Показывает кнопку запроса и скрывает реакции.
        focus: FocusHandle,                 // Начальная точка отсчёта для перехода по Tab.
        selected: Rc<RefCell<Vec<String>>>, // Команды, которые получили обработчики.
    }

    impl Fixture {
        /// Открывает окно с фокусом до кнопок и без выбранной реакции.
        fn new(always: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
            let focus = cx.focus_handle();
            window.focus(&focus);
            Self {
                always,
                focus,
                selected: Rc::default(),
            }
        }
    }

    impl Render for Fixture {
        /// Помещает панель поверх строки простой истории на фиксированной высоте.
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let palette = Palette::for_mode(ThemeMode::Light).with_window_input(window, cx);
            let selected = self.selected.clone();
            let output = selected.clone();
            let command = QuickCommand::on_hover(
                "quick-test-command",
                "Быстрое действие",
                move |_, _| {
                    selected.borrow_mut().push("команда".into());
                },
            );
            let (command, reactions) = if self.always {
                (command.always(Icon::ChatLock, false), None)
            } else {
                let choices = ["❤️", "👍", "👏", "😂", "😮", "😢"]
                    .into_iter()
                    .map(|emoji| QuickReaction {
                        emoji: emoji.into(),
                        glyph: emoji.into(),
                        label: emoji.into(),
                        own: false,
                        pending: false,
                    })
                    .collect();
                (
                    command,
                    QuickReactions::new(choices, move |emoji, _| {
                        output.borrow_mut().push(emoji.into())
                    }),
                )
            };
            div()
                .track_focus(&self.focus)
                .on_action(|_: &NextQuickAction, window, _| window.focus_next())
                .size_full()
                .child(track_focus_input(cx))
                .child(
                    div()
                        .relative()
                        .mt(px(60.))
                        .ml(px(44.))
                        .w(px(144.))
                        .h(px(48.))
                        .bg(palette.message)
                        .debug_selector(|| "quick-test-surface".into())
                        .child(QuickActions {
                            id: ID,
                            own: false,
                            palette,
                            commands: vec![command],
                            reactions,
                        }),
                )
        }
    }

    /// Появляется при наведении, раскрывается с задержкой и выбирает видимый эмодзи.
    #[gpui::test]
    fn hover_opens_quick_reactions_without_resizing_the_bubble(cx: &mut TestAppContext) {
        cx.update(|app| Preferences::set_animations_enabled(app, false).unwrap());
        let clock = cx.executor().clone();
        let (root, ui) = cx.add_window_view(|window, cx| Fixture::new(false, window, cx));
        let surface = ui.debug_bounds("quick-test-surface").unwrap();
        assert!(ui.debug_bounds(CAPSULE_BOUNDS).is_none());
        ui.simulate_mouse_move(surface.center(), None, Modifiers::none());
        ui.run_until_parked();
        let collapsed = ui.debug_bounds(CAPSULE_BOUNDS).unwrap();
        assert_eq!(collapsed.size.height, COLLAPSED);
        assert!(ui.debug_bounds(SECOND_REACTION).is_none());

        ui.simulate_mouse_move(collapsed.center(), None, Modifiers::none());
        ui.run_until_parked();
        clock.advance_clock(OPEN_DELAY);
        ui.run_until_parked();
        let expanded = ui.debug_bounds(CAPSULE_BOUNDS).unwrap();
        assert!(expanded.size.height > collapsed.size.height);
        assert_eq!(expanded.bottom(), collapsed.bottom());
        assert_eq!(Some(surface), ui.debug_bounds("quick-test-surface"));
        let target = ui.debug_bounds(SECOND_REACTION).unwrap().center();
        assert!(expanded.contains(&target));
        ui.simulate_click(target, Modifiers::none());
        ui.run_until_parked();
        ui.read(|app| assert_eq!(&*root.read(app).selected.borrow(), &["👍"]));
        assert_eq!(
            ui.debug_bounds(CAPSULE_BOUNDS).unwrap().size.height,
            COLLAPSED
        );

        ui.simulate_mouse_move(point(px(10.), px(10.)), None, Modifiers::none());
        ui.run_until_parked();
        clock.advance_clock(CLOSE_DELAY);
        ui.run_until_parked();
        // GPUI запоминает последние координаты селектора даже после удаления элемента.
        ui.simulate_click(collapsed.center(), Modifiers::none());
        ui.read(|app| assert_eq!(&*root.read(app).selected.borrow(), &["👍"]));
    }

    /// Раскрывает капсулу вверх и меняет направление без скачка при возврате указателя.
    #[gpui::test]
    fn quick_reactions_reverse_hover_motion_without_moving_the_bubble(cx: &mut TestAppContext) {
        let clock = cx.executor();
        let (root, ui) = cx.add_window_view(|window, cx| Fixture::new(false, window, cx));
        let surface = ui.debug_bounds("quick-test-surface").unwrap();
        ui.simulate_mouse_move(surface.center(), None, Modifiers::none());
        ui.run_until_parked();
        let collapsed = ui.debug_bounds(CAPSULE_BOUNDS).unwrap();
        ui.simulate_mouse_move(collapsed.center(), None, Modifiers::none());
        clock.advance_clock(OPEN_DELAY);
        ui.run_until_parked();
        assert_eq!(ui.debug_bounds(CAPSULE_BOUNDS), Some(collapsed));

        clock.advance_clock(CAPSULE_TRANSITION / 2);
        ui.update(|window, _| window.refresh());
        ui.run_until_parked();
        let opening = ui.debug_bounds(CAPSULE_BOUNDS).unwrap();
        assert!(opening.size.height > collapsed.size.height);
        assert_eq!(opening.bottom(), collapsed.bottom());

        clock.advance_clock(CAPSULE_TRANSITION / 2);
        ui.update(|window, _| window.refresh());
        ui.run_until_parked();
        let expanded = ui.debug_bounds(CAPSULE_BOUNDS).unwrap();
        assert!(expanded.size.height > opening.size.height);
        let target = ui.debug_bounds(SECOND_REACTION).unwrap().center();
        ui.simulate_event(ScrollWheelEvent {
            position: target,
            delta: ScrollDelta::Pixels(point(px(0.), px(16.))),
            ..Default::default()
        });
        ui.run_until_parked();
        let scrolled = ui.debug_bounds(SECOND_REACTION).unwrap().center();
        assert!(scrolled.y > target.y);

        ui.simulate_mouse_move(point(px(10.), px(10.)), None, Modifiers::none());
        clock.advance_clock(CLOSE_DELAY);
        ui.run_until_parked();
        assert_eq!(ui.debug_bounds(SECOND_REACTION).unwrap().center(), scrolled);
        clock.advance_clock(CAPSULE_TRANSITION / 2);
        ui.update(|window, _| window.refresh());
        ui.run_until_parked();
        let closing = ui.debug_bounds(CAPSULE_BOUNDS).unwrap();
        assert!(closing.size.height < expanded.size.height && closing.size.height > COLLAPSED);
        assert_eq!(closing.bottom(), collapsed.bottom());

        ui.simulate_mouse_move(closing.center(), None, Modifiers::none());
        ui.run_until_parked();
        assert_eq!(ui.debug_bounds(CAPSULE_BOUNDS), Some(closing));
        clock.advance_clock(CAPSULE_TRANSITION / 2);
        ui.update(|window, _| window.refresh());
        ui.run_until_parked();
        assert!(ui.debug_bounds(CAPSULE_BOUNDS).unwrap().size.height > closing.size.height);

        clock.advance_clock(CAPSULE_TRANSITION / 2);
        ui.update(|window, _| window.refresh());
        ui.run_until_parked();
        assert_eq!(ui.debug_bounds(CAPSULE_BOUNDS), Some(expanded));
        assert_eq!(ui.debug_bounds("quick-test-surface"), Some(surface));
        let target = ui.debug_bounds(SECOND_REACTION).unwrap().center();
        ui.simulate_click(target, Modifiers::none());
        ui.run_until_parked();
        ui.read(|app| assert_eq!(&*root.read(app).selected.borrow(), &["👍"]));
        assert_eq!(ui.debug_bounds(CAPSULE_BOUNDS), Some(collapsed));
    }

    /// Выключение завершает текущий переход, а включение оставляет капсулу на месте.
    #[gpui::test]
    fn quick_reactions_finish_motion_when_animations_are_disabled(cx: &mut TestAppContext) {
        let clock = cx.executor();
        let (_, ui) = cx.add_window_view(|window, cx| Fixture::new(false, window, cx));
        let surface = ui.debug_bounds("quick-test-surface").unwrap();
        ui.simulate_mouse_move(surface.center(), None, Modifiers::none());
        ui.run_until_parked();
        let collapsed = ui.debug_bounds(CAPSULE_BOUNDS).unwrap();
        ui.simulate_mouse_move(collapsed.center(), None, Modifiers::none());
        clock.advance_clock(OPEN_DELAY);
        ui.run_until_parked();
        assert_eq!(ui.debug_bounds(CAPSULE_BOUNDS), Some(collapsed));

        ui.update(|_, app| Preferences::set_animations_enabled(app, false).unwrap());
        ui.run_until_parked();
        let expanded = ui.debug_bounds(CAPSULE_BOUNDS).unwrap();
        assert!(expanded.size.height > COLLAPSED);
        assert_eq!(expanded.bottom(), collapsed.bottom());

        ui.update(|_, app| Preferences::set_animations_enabled(app, true).unwrap());
        ui.run_until_parked();
        assert_eq!(ui.debug_bounds(CAPSULE_BOUNDS), Some(expanded));
    }

    /// Запрос ключа доступен сразу и не требует наведения на закрытое сообщение.
    #[gpui::test]
    fn always_visible_quick_action_can_be_clicked_without_hovering(cx: &mut TestAppContext) {
        let (root, ui) = cx.add_window_view(|window, cx| Fixture::new(true, window, cx));
        let command = ui.debug_bounds(COMMAND_BOUNDS).unwrap();
        ui.simulate_click(command.center(), Modifiers::none());
        ui.run_until_parked();
        ui.read(|app| assert_eq!(&*root.read(app).selected.borrow(), &["команда"]));
        assert!(ui.debug_bounds(CAPSULE_BOUNDS).is_none());
    }

    /// Раскрывает реакции по Tab, перемещает выбор и сворачивает список по Escape.
    #[gpui::test]
    fn quick_reactions_use_keyboard_focus_and_arrows(cx: &mut TestAppContext) {
        cx.update(|app| app.bind_keys([gpui::KeyBinding::new("tab", NextQuickAction, None)]));
        let (root, ui) = cx.add_window_view(|window, cx| Fixture::new(false, window, cx));
        ui.simulate_keystrokes("tab tab");
        ui.run_until_parked();
        assert!(ui.debug_bounds(SECOND_REACTION).is_some());

        ui.simulate_keystrokes("escape");
        ui.run_until_parked();
        assert_eq!(
            ui.debug_bounds(CAPSULE_BOUNDS).unwrap().size.height,
            COLLAPSED
        );
        ui.simulate_keystrokes("down down");
        ui.run_until_parked();
        assert!(ui.debug_bounds(SECOND_REACTION).is_some());
        ui.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("enter").unwrap(),
        });
        ui.run_until_parked();
        ui.read(|app| assert_eq!(&*root.read(app).selected.borrow(), &["👍"]));
        assert_eq!(
            ui.debug_bounds(CAPSULE_BOUNDS).unwrap().size.height,
            COLLAPSED
        );
    }
}
