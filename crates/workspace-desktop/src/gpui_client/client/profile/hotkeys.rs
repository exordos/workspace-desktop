//! Диалог горячих клавиш, его фильтр и окно записи нового сочетания.

use std::collections::HashMap;

use gpui::{
    Context, Div, Entity, FocusHandle, Keystroke, ScrollHandle, Stateful, Subscription, WeakEntity,
    Window, div, prelude::*, px, rgb,
};
use workspace_core::context::OwnerContext;

use crate::gpui_client::{
    preferences::{
        Language, Preferences,
        shortcuts::{
            BindingError, CancelRecording, FIXED, SHORTCUT_GROUPS, Shortcut, binding_from_key,
            format, validate,
        },
    },
    right_panel::RightPanel,
    ui::{Button, ButtonAppearance, Icon, Palette, TextInput, TextInputChanged, activate},
};

/// Ввод пользователя, пока открыт редактор одного сочетания.
struct Recording {
    shortcut: Shortcut,          // Команда, для которой меняют клавиши.
    opener: FocusHandle,         // Строка, в которую возвращается фокус.
    candidate: Option<String>,   // Новая клавиша; None, пока пользователь не нажал её.
    error: Option<BindingError>, // Причина, по которой сочетание нельзя сохранить.
}

/// Диалог выбора и редактирования сочетаний для активного входа.
pub(in crate::gpui_client::client) struct Hotkeys {
    host: WeakEntity<RightPanel>, // Слой окна, который закрывает диалог.
    owner: OwnerContext,          // Владелец назначений в открытом диалоге.
    search: Entity<TextInput>,    // Ввод для фильтра по названию действия.
    scroll: ScrollHandle,         // Положение списка при редактировании.
    focus: HashMap<&'static str, FocusHandle>, // Фокус кнопок этого окна.
    record_focus: FocusHandle,    // Поле, которое перехватывает сочетание при записи.
    record_region: FocusHandle,   // Область вложенного редактора для обхода клавишей Tab.
    record_end: FocusHandle,      // Точка после последней кнопки для обратного обхода.
    _record_keys: Subscription,   // Перехватывает клавишу раньше системных действий GPUI.
    recording: Option<Recording>, // Текущая форма или список без редактора.
    storage_error: bool,          // Последнее изменение не удалось сохранить.
}

impl Hotkeys {
    /// Создаёт список и подписывает отрисовку на изменение поисковой строки.
    pub(super) fn new(
        host: WeakEntity<RightPanel>,
        owner: OwnerContext,
        cx: &mut Context<Self>,
    ) -> Self {
        let language = Preferences::read(cx).language;
        let search = cx.new(|cx| {
            TextInput::new(
                "profile-hotkeys-search",
                language.tr("hotkeys.search"),
                false,
                cx,
            )
        });
        cx.subscribe(&search, |_, _, _: &TextInputChanged, cx| cx.notify())
            .detach();
        let target = cx.weak_entity();
        let record_keys = cx.intercept_keystrokes(move |event, window, app| {
            let _ = target.update(app, |view, cx| {
                view.record_key(&event.keystroke, window, cx)
            });
        });
        Self {
            host,
            owner,
            search,
            scroll: ScrollHandle::new(),
            focus: HashMap::new(),
            record_focus: cx.focus_handle().tab_stop(true),
            record_region: cx.focus_handle(),
            record_end: cx.focus_handle(),
            _record_keys: record_keys,
            recording: None,
            storage_error: false,
        }
    }

    /// Возвращает стабильный фокус кнопки при фильтрации и повторной отрисовке.
    fn focus(&mut self, id: &'static str, cx: &mut Context<Self>) -> FocusHandle {
        self.focus
            .entry(id)
            .or_insert_with(|| cx.focus_handle().tab_stop(true))
            .clone()
    }

    /// Показывает кнопку закрытия списка или редактора.
    fn close_button(
        &mut self,
        palette: Palette,
        editor: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let id = if editor {
            "profile-hotkey-editor-close"
        } else {
            "profile-hotkeys-close"
        };
        let focus = self.focus(id, cx);
        let weak = cx.weak_entity();
        activate(
            div()
                .id(id)
                .debug_selector(move || id.into())
                .p(px(4.))
                .rounded(px(4.))
                .hover(|style| style.bg(palette.background))
                .child(Icon::Close.element(px(20.), palette.text_muted)),
            (editor || self.recording.is_none()).then_some(&focus),
            palette,
            move |window, app| {
                let _ = weak.update(app, |view, cx| {
                    if editor {
                        view.close_recording(window, cx);
                    } else {
                        let _ = view
                            .host
                            .update(cx, |panel, cx| panel.close_modal(window, cx));
                    }
                });
            },
        )
    }

    /// Открывает запись поверх списка и передаёт фокус полю сочетания.
    fn open_recording(
        &mut self,
        shortcut: Shortcut,
        opener: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.recording = Some(Recording {
            shortcut,
            opener,
            candidate: None,
            error: None,
        });
        self.storage_error = false;
        self.search
            .update(cx, |view, cx| view.set_enabled(false, cx));
        window.focus(&self.record_focus);
        cx.notify();
    }

    /// Отменяет запись и возвращает фокус в выделявшуюся строку.
    fn close_recording(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(recording) = self.recording.take() {
            self.storage_error = false;
            self.search
                .update(cx, |view, cx| view.set_enabled(true, cx));
            window.focus(&recording.opener);
            cx.notify();
        }
    }

    /// Удерживает Tab внутри вложенной формы, пока список виден под ней.
    fn cycle_recording(&self, reverse: bool, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        if reverse {
            window.focus_prev();
            if !self.record_region.contains_focused(window, cx) {
                window.focus(&self.record_end);
                window.focus_prev();
            }
        } else {
            window.focus_next();
            if !self.record_region.contains_focused(window, cx) {
                window.focus(&self.record_region);
                window.focus_next();
            }
        }
    }

    /// Запоминает клавишу и сразу показывает ошибку рядом с полем.
    fn record_key(&mut self, key: &Keystroke, window: &mut Window, cx: &mut Context<Self>) {
        if !self.record_focus.is_focused(window) || matches!(key.key.as_str(), "escape" | "tab") {
            return;
        }
        let Some(recording) = self.recording.as_mut() else {
            return;
        };
        window.prevent_default();
        cx.stop_propagation();
        let candidate = binding_from_key(key);
        if recording.candidate.as_deref() == Some(&candidate) {
            return;
        }
        recording.error = validate(recording.shortcut, &candidate, Preferences::read(cx)).err();
        recording.candidate = Some(candidate);
        self.storage_error = false;
        cx.notify();
    }

    /// Меняет назначения и оставляет видимый текст ошибки при сбое записи.
    fn change(
        &mut self,
        cx: &mut Context<Self>,
        update: impl FnOnce(&mut crate::gpui_client::preferences::PreferenceValues),
    ) -> bool {
        if !Preferences::is_selected(cx, &self.owner) {
            return false;
        }
        self.storage_error = Preferences::update(cx, update).is_err();
        cx.notify();
        !self.storage_error
    }

    /// Сохраняет записанную клавишу, отключает её или возвращает стандартное сочетание.
    fn edit(&mut self, action: Edit, window: &mut Window, cx: &mut Context<Self>) {
        let Some(recording) = self.recording.as_mut() else {
            return;
        };
        let shortcut = recording.shortcut;
        let value = match action {
            Edit::Save => {
                let Some(candidate) = recording.candidate.clone() else {
                    return;
                };
                if let Err(error) = validate(shortcut, &candidate, Preferences::read(cx)) {
                    recording.error = Some(error);
                    cx.notify();
                    return;
                }
                Some(Some(candidate))
            }
            Edit::Disable => Some(None),
            Edit::Reset => {
                if let Some(binding) = shortcut.binding
                    && let Err(error) = validate(shortcut, binding, Preferences::read(cx))
                {
                    recording.error = Some(error);
                    cx.notify();
                    return;
                }
                None
            }
        };
        if self.change(cx, move |settings| {
            if let Some(value) = value {
                settings.hotkeys.insert(shortcut.id.into(), value);
            } else {
                settings.hotkeys.remove(shortcut.id);
            }
        }) {
            self.close_recording(window, cx);
        }
    }

    /// Рисует строку сочетания и открывает редактор для изменяемых команд.
    fn shortcut_row(
        &mut self,
        shortcut: Shortcut,
        language: Language,
        palette: Palette,
        fixed: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let selection = if fixed {
            shortcut.binding
        } else {
            shortcut.binding(Preferences::read(cx))
        };
        let value = selection
            .map(format)
            .unwrap_or_else(|| language.tr("hotkeys.disabled").into());
        let id = shortcut.id;
        let row = div()
            .id(id)
            .debug_selector(move || format!("profile-hotkey-{id}"))
            .w_full()
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(8.))
            .px(px(8.))
            .py(px(6.))
            .rounded(px(4.))
            .bg(palette.background)
            .text_size(px(12.))
            .line_height(px(16.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .min_w_0()
                            .when(!fixed, |label| label.truncate())
                            .debug_selector(move || format!("profile-hotkey-label-{id}"))
                            .child(shortcut.title(language)),
                    )
                    .children(
                        shortcut
                            .hint(language)
                            .map(|hint| div().text_color(palette.text_secondary).child(hint)),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .font_family("Menlo")
                    .text_size(px(11.))
                    .line_height(px(15.))
                    .px(px(6.))
                    .py(px(1.))
                    .rounded(px(4.))
                    .border_1()
                    .border_color(palette.border)
                    .bg(palette.card)
                    .child(value),
            );
        if fixed {
            return row;
        }
        let focus = self.focus(id, cx);
        let opener = focus.clone();
        let weak = cx.weak_entity();
        activate(
            row.hover(|style| style.bg(palette.card_hover)),
            self.recording.is_none().then_some(&focus),
            palette,
            move |window, app| {
                let _ = weak.update(app, |view, cx| {
                    view.open_recording(shortcut, opener.clone(), window, cx)
                });
            },
        )
    }

    /// Показывает подходящие поиску сочетания одной группы.
    fn section(
        &mut self,
        title: &'static str,
        shortcuts: &[Shortcut],
        query: &str,
        language: Language,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let rows: Vec<_> = shortcuts
            .iter()
            .copied()
            .filter(|shortcut| shortcut.title(language).to_lowercase().contains(query))
            .map(|shortcut| {
                self.shortcut_row(
                    shortcut,
                    language,
                    palette,
                    shortcut.id.starts_with("fixed."),
                    cx,
                )
            })
            .collect();
        if rows.is_empty() {
            return None;
        }
        Some(
            div()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(
                    div()
                        .text_size(px(12.))
                        .line_height(px(16.))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(palette.text_muted)
                        .child(title.to_uppercase()),
                )
                .child(div().flex().flex_col().gap(px(6.)).children(rows)),
        )
    }

    /// Рисует общий переключатель и кнопку восстановления всех назначений.
    fn controls(&mut self, language: Language, palette: Palette, cx: &mut Context<Self>) -> Div {
        let enabled = Preferences::read(cx).hotkeys_enabled;
        let interactive = self.recording.is_none();
        let toggle = self.focus("profile-hotkeys-enabled", cx);
        let reset = self.focus("profile-hotkeys-reset-all", cx);
        let on_toggle = cx.weak_entity();
        let on_reset = on_toggle.clone();
        let switch = activate(
            div()
                .id("profile-hotkeys-enabled")
                .debug_selector(|| "profile-hotkeys-enabled".into())
                .flex()
                .items_center()
                .gap(px(4.))
                .child(
                    div()
                        .size(px(13.))
                        .rounded(px(2.))
                        .border_1()
                        .border_color(if enabled {
                            rgb(0x007aff).into()
                        } else {
                            palette.text_muted
                        })
                        .bg(if enabled {
                            rgb(0x007aff).into()
                        } else {
                            palette.surface
                        })
                        .flex()
                        .items_center()
                        .justify_center()
                        .when(enabled, |element| {
                            element.child(Icon::Check.element(px(11.), rgb(0xffffff).into()))
                        }),
                )
                .child(language.tr("hotkeys.enabled")),
            interactive.then_some(&toggle),
            palette,
            move |_, app| {
                let _ = on_toggle.update(app, |view, cx| {
                    view.change(cx, |settings| {
                        settings.hotkeys_enabled = !settings.hotkeys_enabled
                    });
                });
            },
        );
        let mut button = Button::new("profile-hotkeys-reset-all", language.tr("hotkeys.resetAll"));
        button.appearance = ButtonAppearance::Ghost;
        div()
            .flex()
            .flex_wrap()
            .justify_between()
            .items_center()
            .gap(px(8.))
            .child(switch)
            .child(
                button
                    .render(palette, interactive.then_some(&reset), move |_, app| {
                        let _ = on_reset.update(app, |view, cx| {
                            view.change(cx, |settings| settings.hotkeys.clear());
                        });
                    })
                    .debug_selector(|| "profile-hotkeys-reset-all".into())
                    .border_color(palette.accent)
                    .text_color(palette.accent)
                    .h(px(32.)),
            )
    }

    /// Рисует поле записи и три действия редактора поверх диалога списка.
    fn recording(
        &mut self,
        language: Language,
        palette: Palette,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<Stateful<Div>> {
        let selected = self.recording.as_ref()?;
        let title = format!(
            "{}: {}",
            language.tr("hotkeys.edit"),
            selected.shortcut.title(language)
        );
        let entered = selected.candidate.is_some();
        let display = selected
            .candidate
            .as_deref()
            .map(format)
            .unwrap_or_else(|| language.tr("hotkeys.recordInput").into());
        let error = selected
            .error
            .map(|error| error.message(language))
            .or_else(|| {
                self.storage_error
                    .then(|| language.tr("hotkeys.storageError"))
            });
        let close = self.close_button(palette, true, cx);
        let actions = [
            (
                Edit::Save,
                "profile-hotkey-save",
                language.tr("common.save"),
                ButtonAppearance::Primary,
            ),
            (
                Edit::Disable,
                "profile-hotkey-disable",
                language.tr("hotkeys.disable"),
                ButtonAppearance::Ghost,
            ),
            (
                Edit::Reset,
                "profile-hotkey-reset",
                language.tr("hotkeys.reset"),
                ButtonAppearance::Ghost,
            ),
        ];
        let overlay = div()
            .id("profile-hotkey-editor-layer")
            .absolute()
            .inset_0()
            .flex()
            .justify_center()
            .items_start()
            .pt(window.viewport_size().height * 0.2)
            .child(
                div()
                    .id("profile-hotkey-editor-backdrop")
                    .absolute()
                    .inset_0()
                    .bg(gpui::rgba(0x00000080))
                    .occlude()
                    .on_click(cx.listener(|view, _, window, cx| view.close_recording(window, cx))),
            );
        let editor = div()
            .id("profile-hotkey-editor")
            .debug_selector(|| "profile-hotkey-editor".into())
            .track_focus(&self.record_region)
            .key_context("WorkspaceHotkeyRecording")
            .on_action(cx.listener(|view, _: &CancelRecording, window, cx| {
                cx.stop_propagation();
                view.close_recording(window, cx);
            }))
            .on_action(
                cx.listener(|view, _: &crate::gpui_client::NextControl, window, cx| {
                    view.cycle_recording(false, window, cx);
                }),
            )
            .on_action(cx.listener(
                |view, _: &crate::gpui_client::PreviousControl, window, cx| {
                    view.cycle_recording(true, window, cx);
                },
            ))
            .occlude()
            .w_full()
            .max_w(px(448.))
            .rounded(px(12.))
            .border_1()
            .border_color(palette.border)
            .bg(palette.surface)
            .shadow_lg()
            .p(px(24.))
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_start()
                    .gap(px(12.))
                    .child(
                        div()
                            .text_size(px(16.))
                            .line_height(px(24.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(close),
            )
            .child(
                div()
                    .text_size(px(14.))
                    .line_height(px(20.))
                    .text_color(palette.text_secondary)
                    .child(language.tr("hotkeys.record")),
            )
            .child(
                div()
                    .id("profile-hotkey-record")
                    .debug_selector(|| "profile-hotkey-record".into())
                    .track_focus(&self.record_focus)
                    .w_full()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(palette.accent)
                    .bg(palette.field)
                    .p(px(8.))
                    .text_color(if entered {
                        palette.text
                    } else {
                        palette.text_secondary
                    })
                    .child(display),
            )
            .children(error.map(|message| div().text_color(palette.danger).child(message)))
            .child(div().flex().flex_wrap().gap(px(8.)).children(actions.map(
                |(action, id, label, appearance)| {
                    let focus = self.focus(id, cx);
                    let weak = cx.weak_entity();
                    let mut button = Button::new(id, label);
                    button.appearance = appearance;
                    button.disabled = action == Edit::Save
                        && (!entered
                            || self
                                .recording
                                .as_ref()
                                .is_some_and(|item| item.error.is_some()));
                    button
                        .render(palette, Some(&focus), move |window, app| {
                            let _ = weak.update(app, |view, cx| view.edit(action, window, cx));
                        })
                        .debug_selector(move || id.into())
                        .h(px(36.))
                        .px(px(16.))
                        .when(appearance == ButtonAppearance::Ghost, |element| {
                            element.border_color(palette.border)
                        })
                },
            )))
            .child(
                div()
                    .id("profile-hotkey-editor-end")
                    .track_focus(&self.record_end),
            );
        Some(overlay.child(editor))
    }
}

/// Действия формы записи по выбранной команде.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Edit {
    Save,    // Применить записанное сочетание.
    Disable, // Оставить действие без сочетания.
    Reset,   // Вернуть исходное сочетание.
}

impl Render for Hotkeys {
    /// Показывает список в двух колонках начиная с 768 px и одну на узком окне.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        let palette = Palette::for_window(window, cx);
        let language = Preferences::read(cx).language;
        let t = |key| language.tr(key);
        let query = self.search.read(cx).text().trim().to_lowercase();
        let groups = [
            (t("hotkeys.navigation"), SHORTCUT_GROUPS[0]),
            (t("hotkeys.interface"), SHORTCUT_GROUPS[1]),
            (t("hotkeys.composer"), SHORTCUT_GROUPS[2]),
            (t("hotkeys.fixedTitle"), FIXED.as_slice()),
        ];
        let mut sections: Vec<_> = groups
            .into_iter()
            .filter_map(|(title, shortcuts)| {
                self.section(title, shortcuts, &query, language, palette, cx)
            })
            .collect();
        let mut grid = div().w_full().flex_none().flex().flex_col().gap(px(16.));
        if sections.is_empty() {
            grid = grid.child(t("hotkeys.empty"));
        } else if viewport.width >= px(768.) {
            let mut rows = sections.drain(..);
            while let Some(left) = rows.next() {
                let right = rows.next().unwrap_or_else(div);
                grid = grid.child(
                    div()
                        .flex()
                        .items_start()
                        .gap(px(16.))
                        .child(left.flex_1())
                        .child(right.flex_1()),
                );
            }
        } else {
            grid = grid.children(
                sections
                    .into_iter()
                    .map(|section| section.w_full().flex_none()),
            );
        }
        let overlay = self.recording(language, palette, window, cx);
        let search = div().relative().child(self.search.clone()).child(
            div()
                .absolute()
                .right(px(8.))
                .top(px(7.))
                .child(Icon::Search.element(px(18.), palette.text_muted)),
        );
        let dialog = div()
            .id("profile-hotkeys-dialog")
            .debug_selector(|| "profile-hotkeys-dialog".into())
            .w(viewport.width.min(px(768.)))
            .max_h(viewport.height * 0.92)
            .rounded(px(12.))
            .overflow_hidden()
            .border_1()
            .border_color(palette.border)
            .bg(palette.surface)
            .shadow_lg()
            .flex()
            .flex_col()
            .text_color(palette.text)
            .child(
                div()
                    .flex_none()
                    .pt(px(24.))
                    .px(px(24.))
                    .pb(px(16.))
                    .flex()
                    .justify_between()
                    .items_start()
                    .gap(px(12.))
                    .child(
                        div()
                            .text_size(px(16.))
                            .line_height(px(24.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(t("hotkeys.title")),
                    )
                    .child(self.close_button(palette, false, cx)),
            )
            .child(
                div()
                    .id("profile-hotkeys-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .px(px(40.))
                    .pt(px(16.))
                    .pb(px(40.))
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .child(
                        div()
                            .text_size(px(14.))
                            .line_height(px(20.))
                            .text_color(palette.text_secondary)
                            .child(t("hotkeys.localDescription")),
                    )
                    .child(search)
                    .child(self.controls(language, palette, cx))
                    .when(self.storage_error && self.recording.is_none(), |body| {
                        body.child(
                            div()
                                .text_color(palette.danger)
                                .child(t("hotkeys.storageError")),
                        )
                    })
                    .child(grid),
            );
        div()
            .relative()
            .flex()
            .items_center()
            .justify_center()
            .when(overlay.is_some(), |layer| {
                layer.w(viewport.width).h(viewport.height)
            })
            .child(dialog)
            .children(overlay)
    }
}
