//! Рисует кнопки папок, контекстное меню и быстрый выбор в двух размещениях.
//! Взаимодействия передаются состоянию панели, общие примитивы берутся из UI-кита.

use gpui::{
    Context, Div, ElementId, MouseButton, Render, ScrollHandle, SharedString, Stateful, Window,
    canvas, div, point, prelude::*, px, rgb,
};
use uuid::Uuid;
use workspace_core::messenger::folders::{ALL_CHATS_ID, FolderKind};

use super::{
    super::super::ui::{
        BadgeTone, Button, ButtonAppearance, Icon, Palette, TextRole, label,
        popup::{menu_layer, menu_navigation, menu_surface, tooltip},
        unread_badge,
    },
    CloseQuickList, FolderRail, KeepQuickFocus, NextFolder, OpenMenu, Overlay, PreviousFolder,
    RailAction, RailEntry,
};

impl FolderRail {
    /// Рисует кнопку папки и добавляет правый клик и клавишу контекстного меню.
    fn folder_button(
        &mut self,
        entry: &RailEntry,
        scroll: &ScrollHandle,
        index: usize,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let id = entry.id;
        let serial = self.serial;
        let vertical = self.state.vertical;
        let selected = self.state.selected == id;
        let focus = self.focuses.entry(id).or_insert_with(|| cx.focus_handle());
        let mut button = Button::new(ElementId::Uuid(id), "");
        button.appearance = ButtonAppearance::Ghost;
        let rail = cx.entity().downgrade();
        let title: SharedString = entry.title_for(cx).to_owned().into();
        let text = label(palette, TextRole::Caption, title.clone())
            .text_color(if selected {
                palette.text
            } else {
                palette.text_secondary
            })
            .truncate();
        let color = entry
            .color
            .filter(|_| entry.kind == FolderKind::Custom)
            .map(|color| rgb(color & 0x00ff_ffff).into());
        let icon = folder_icon(entry.kind, selected).element(
            if vertical && selected {
                px(32.)
            } else if vertical {
                px(29.)
            } else {
                px(18.)
            },
            color.unwrap_or(if selected { palette.text } else { palette.icon }),
        );
        let scroll = scroll.clone();
        let button_palette = Palette {
            card_hover: if selected {
                gpui::transparent_black()
            } else {
                color.map_or(palette.card_hover, |color| color.opacity(0.10))
            },
            ..palette
        };
        button
            .render(button_palette, Some(focus), move |_, app| {
                let _ = rail.update(app, |rail, cx| rail.select(serial, id, cx));
            })
            .relative()
            .flex_none()
            .debug_selector(move || format!("folder-rail-entry-{id}"))
            .key_context("FolderRailButton")
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |rail, event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    rail.open_menu(serial, id, event.position, window, cx);
                }),
            )
            .on_action(cx.listener(move |rail, _: &OpenMenu, window, cx| {
                if let Some(bounds) = scroll.bounds_for_item(index) {
                    let anchor = point(bounds.left(), bounds.bottom()) + scroll.offset();
                    rail.open_menu(serial, id, anchor, window, cx);
                }
            }))
            .tooltip(move |_, app| tooltip(palette, title.clone(), app))
            .when(vertical, |button| {
                button.w(px(64.)).h(px(72.)).px_1().flex_col().gap(px(2.))
            })
            .child(icon)
            .child(if vertical {
                text.w_full().text_center()
            } else {
                text.max_w(px(112.))
            })
            .children(
                entry
                    .unread
                    .and_then(|count| unread_badge(palette, u64::from(count), BadgeTone::Accent))
                    .map(|badge| {
                        badge
                            .absolute()
                            .right(px(2.))
                            .top(if vertical { px(3.) } else { px(-4.) })
                    }),
            )
    }

    /// Рисует создание только для доступа текущего аккаунта, сохраняя кнопку при обновлении.
    fn create_button(&self, palette: Palette, cx: &Context<Self>) -> Stateful<Div> {
        let mut button = Button::new("folder-rail-create", "");
        button.appearance = ButtonAppearance::Ghost;
        button.disabled = !self.state.can_manage;
        let serial = self.serial;
        let rail = cx.entity().downgrade();
        button
            .render(palette, None, move |_, app| {
                let _ = rail.update(app, |rail, cx| {
                    if rail.serial == serial && rail.state.can_manage {
                        cx.emit(RailAction::Create);
                    }
                });
            })
            .w(px(32.))
            .flex_none()
            .child(Icon::Add.element(
                px(24.),
                if self.state.can_manage {
                    palette.icon_hover
                } else {
                    palette.disabled
                },
            ))
            .tooltip(move |_, app| tooltip(palette, "Создать папку".into(), app))
            .debug_selector(|| "folder-rail-create".into())
    }

    /// Рисует кнопку быстрого выбора рядом с прокручиваемой областью.
    fn quick_trigger(&self, palette: Palette, cx: &Context<Self>) -> Stateful<Div> {
        let mut button = Button::new("folder-rail-quick", "");
        button.appearance = ButtonAppearance::Ghost;
        let rail = cx.entity().downgrade();
        let serial = self.serial;
        button
            .render(palette, Some(&self.quick_focus), move |window, app| {
                let _ = rail.update(app, |rail, cx| rail.open_quick(serial, window, cx));
            })
            .w(px(32.))
            .flex_none()
            .child(Icon::FolderList.element(px(24.), palette.icon_hover))
            .tooltip(move |_, app| tooltip(palette, "Список папок".into(), app))
            .debug_selector(|| "folder-rail-quick".into())
    }

    /// Следит за реальными границами после разметки и показывает быстрый выбор при переполнении.
    fn overflow_measurement(&self, cx: &Context<Self>) -> impl IntoElement {
        let rail = cx.entity().downgrade();
        let scroll = self.scroll.clone();
        let overflow = self.overflow;
        let serial = self.serial;
        canvas(
            |_, _, _| (),
            move |_, (), window, app| {
                // Кнопка отбора занимает 32 пикселя. Проверяем, поместились бы строки без неё.
                let occupied = if overflow { px(32.) } else { px(0.) };
                let needed = scroll.max_offset().height > occupied;
                if needed != overflow {
                    window.defer(app, move |window, app| {
                        let _ = rail.update(app, |rail, cx| {
                            if rail.serial == serial && rail.state.vertical {
                                rail.overflow = needed;
                                if !needed && matches!(rail.overlay, Some(Overlay::Quick { .. })) {
                                    rail.close_overlay(window, cx);
                                }
                                cx.notify();
                            }
                        });
                    });
                }
            },
        )
        .absolute()
        .size_0()
    }

    /// Размещает «Все чаты» сверху и прокручивает остальные кнопки независимо от каталога.
    fn vertical(&mut self, palette: Palette, cx: &mut Context<Self>) -> Stateful<Div> {
        let entries: Vec<_> = self.state.visible().cloned().collect();
        let first = self.first.clone();
        let scroll = self.scroll.clone();
        let mut rest = div()
            .id("folder-rail-vertical-scroll")
            .w_full()
            .min_h_0()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(2.))
            .overflow_y_scroll()
            .track_scroll(&scroll);
        for (index, entry) in entries.iter().skip(1).enumerate() {
            rest = rest.child(self.folder_button(entry, &scroll, index, palette, cx));
        }
        rest = rest.child(self.create_button(palette, cx));
        div()
            .id("folder-rail-vertical")
            .debug_selector(|| "folder-rail-vertical".into())
            .w(px(72.))
            .h_full()
            .min_h_0()
            .flex_none()
            .flex()
            .flex_col()
            .items_center()
            .py(px(8.))
            .relative()
            .child(
                div()
                    .id("folder-rail-first")
                    .flex_none()
                    .track_scroll(&first)
                    .child(self.folder_button(&entries[0], &first, 0, palette, cx)),
            )
            .child(rest)
            .when(self.overflow, |rail| {
                rail.child(
                    div()
                        .id("folder-rail-quick-anchor")
                        .flex_none()
                        .track_scroll(&self.quick_button)
                        .child(self.quick_trigger(palette, cx)),
                )
            })
            .child(self.overflow_measurement(cx))
    }

    /// Показывает тот же состав в одну горизонтальную прокручиваемую строку.
    fn horizontal(&mut self, palette: Palette, cx: &mut Context<Self>) -> Stateful<Div> {
        let entries: Vec<_> = self.state.visible().cloned().collect();
        let scroll = self.scroll.clone();
        let mut rail = div()
            .id("folder-rail-horizontal")
            .debug_selector(|| "folder-rail-horizontal".into())
            .w_full()
            .h(px(44.))
            .min_w_0()
            .flex_none()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .overflow_x_scroll()
            .track_scroll(&scroll);
        for (index, entry) in entries.iter().enumerate() {
            rail = rail.child(self.folder_button(entry, &scroll, index, palette, cx));
        }
        rail.child(self.create_button(palette, cx))
    }

    /// Рисует контекстное меню в общем всплывающем слое окна.
    fn menu_surface(
        &self,
        id: Uuid,
        active: usize,
        palette: Palette,
        window: &Window,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let items = self.menu_items(id);
        let mut menu = menu_surface(
            "folder-rail-menu",
            palette,
            window.viewport_size(),
            px(242.),
            px(244.),
        )
        .debug_selector(|| "folder-rail-menu".into())
        .on_mouse_down_out(cx.listener(|rail, _, window, cx| {
            cx.stop_propagation();
            rail.close_overlay(window, cx);
        }));
        let active = items
            .get(active)
            .filter(|item| item.enabled)
            .map(|item| item.action)
            .or_else(|| {
                items
                    .iter()
                    .find(|item| item.enabled)
                    .map(|item| item.action)
            });
        for item in items {
            let mut button = Button::new(item.id, "");
            button.appearance = ButtonAppearance::Ghost;
            button.disabled = !item.enabled;
            let action = item.action;
            let rail = cx.entity().downgrade();
            let serial = self.serial;
            let tone = if !item.enabled {
                palette.text_muted
            } else if matches!(action, RailAction::Delete(_)) {
                palette.danger
            } else {
                palette.text
            };
            menu = menu.child(
                button
                    .render(palette, None, move |window, app| {
                        let _ = rail.update(app, |rail, cx| {
                            if rail.serial == serial {
                                rail.pick_menu(action, window, cx);
                            }
                        });
                    })
                    .w_full()
                    .min_w_0()
                    .flex_none()
                    .justify_start()
                    .when(active == Some(action), |row| row.bg(palette.card_selected))
                    .child(item.icon.element(px(16.), tone))
                    .child(
                        label(palette, TextRole::Caption, item.label)
                            .text_color(tone)
                            .truncate(),
                    )
                    .debug_selector(move || item.id.into()),
            );
        }
        menu
    }

    /// Рисует поиск и подсвеченный список доступных папок с их счётчиками.
    fn quick_surface(
        &self,
        palette: Palette,
        window: &Window,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let rows = self.quick_entries(cx);
        let active = self.quick_active(&rows);
        let mut list = div()
            .id("folder-rail-quick-scroll")
            .w_full()
            .min_h_0()
            .max_h(px(256.))
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .track_scroll(&self.quick_scroll);
        for (index, entry) in rows.iter().enumerate() {
            let id = entry.id;
            let mut button = Button::new(ElementId::Uuid(id), "");
            button.appearance = ButtonAppearance::Ghost;
            let rail = cx.entity().downgrade();
            let serial = self.serial;
            let icon_color = entry
                .color
                .filter(|_| entry.kind == FolderKind::Custom)
                .map(|color| rgb(color & 0x00ff_ffff).into())
                .unwrap_or(palette.accent);
            list = list.child(
                button
                    .render(palette, None, move |window, app| {
                        let _ = rail.update(app, |rail, cx| {
                            if rail.serial == serial {
                                rail.pick_quick(id, window, cx);
                            }
                        });
                    })
                    .w_full()
                    .min_w_0()
                    .flex_none()
                    .justify_start()
                    .when(active == Some(index), |row| row.bg(palette.card_selected))
                    .child(
                        folder_icon(entry.kind, self.state.selected == id)
                            .element(px(18.), icon_color),
                    )
                    .child(
                        label(palette, TextRole::Caption, entry.title_for(cx).to_owned())
                            .flex_1()
                            .min_w_0()
                            .truncate(),
                    )
                    .children(entry.unread.and_then(|count| {
                        unread_badge(palette, u64::from(count), BadgeTone::Accent)
                    }))
                    .debug_selector(move || format!("folder-rail-quick-entry-{id}")),
            );
        }
        if rows.is_empty() {
            let locale = crate::gpui_client::preferences::Preferences::read(cx).language;
            list = list.child(
                label(
                    palette,
                    TextRole::Caption,
                    locale.text("Папки не найдены", "No folders found"),
                )
                .px_2()
                .py_1()
                .debug_selector(|| "folder-rail-quick-empty".into()),
            );
        }
        menu_surface(
            "folder-rail-quick-list",
            palette,
            window.viewport_size(),
            px(272.),
            px(304.),
        )
        .debug_selector(|| "folder-rail-quick-list".into())
        .key_context("FolderQuickList")
        .gap_1()
        .on_action(cx.listener(|rail, _: &PreviousFolder, _, cx| rail.move_quick(-1, cx)))
        .on_action(cx.listener(|rail, _: &NextFolder, _, cx| rail.move_quick(1, cx)))
        .on_action(
            cx.listener(|rail, _: &CloseQuickList, window, cx| rail.close_overlay(window, cx)),
        )
        .on_action(|_: &KeepQuickFocus, _, cx| cx.stop_propagation())
        .on_mouse_down_out(cx.listener(|rail, _, window, cx| {
            cx.stop_propagation();
            rail.close_overlay(window, cx);
        }))
        .child(self.query.clone())
        .child(list)
    }
}

impl Render for FolderRail {
    /// Показывает выбранное размещение и поверх него открытое меню, если оно есть.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_window(window, cx);
        let panel = if self.state.vertical {
            self.vertical(palette, cx)
        } else {
            self.horizontal(palette, cx)
        };
        if self.overlay.is_none()
            && (self.menu_focus.is_focused(window) || self.query.read(cx).is_focused(window))
            && let Some(focus) = self
                .focuses
                .get(&self.state.selected)
                .or_else(|| self.focuses.get(&ALL_CHATS_ID))
        {
            // Снимок мог удалить кнопку открытого меню, пока фокус находился в нём.
            window.focus(focus);
        }
        let viewport = window.viewport_size();
        match self.overlay {
            Some(Overlay::Menu { id, active, anchor }) => {
                let menu = self.menu_surface(id, active, palette, window, cx);
                menu_navigation(panel, &self.menu_focus, cx, Self::menu_input)
                    .children(menu_layer(viewport, anchor, menu))
            }
            Some(Overlay::Quick { anchor, .. }) => {
                let menu = self.quick_surface(palette, window, cx);
                panel.children(menu_layer(viewport, anchor, menu))
            }
            None => panel,
        }
    }
}

/// Выбирает значок системной папки или отдельный вид открытой пользовательской папки.
fn folder_icon(kind: FolderKind, selected: bool) -> Icon {
    match kind {
        FolderKind::All => Icon::Folders,
        FolderKind::Personal => Icon::Profile,
        FolderKind::Channels => Icon::Channels,
        FolderKind::Custom | FolderKind::Unknown if selected => Icon::FolderOpen,
        FolderKind::Custom | FolderKind::Unknown => Icon::Folder,
    }
}
