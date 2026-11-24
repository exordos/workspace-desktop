//! Даёт владельцу канала поиск, выбор и добавление участников из справочника.

use std::collections::HashSet;

use gpui::{
    Context, Div, Entity, IntoElement, Render, Subscription, Task, WeakEntity, Window, div,
    prelude::*, px,
};
use uuid::Uuid;
use workspace_core::{directory::UserProfile, messenger::details::SidebarDetails};

use super::{Conversation, Mutation, MutationKind, Panel, State, action, source};
use crate::gpui_client::{
    preferences::Preferences,
    right_panel::RightPanel,
    ui::{
        Avatar, AvatarPresence, Button, ButtonAppearance, Icon, Palette, SURFACE_RADIUS, TextInput,
        TextInputChanged, TextRole, activate, label,
    },
};

/// Справочник и выбранные пользователи в одном открытии панели приглашения.
pub(super) struct Picker {
    input: Entity<TextInput>,                // Фильтр по имени и почте.
    _change: Subscription,                   // Перестраивает список при изменении фильтра.
    request: Option<(Uuid, Task<()>)>,       // Действующий запрос справочника.
    users: Option<Vec<(Uuid, UserProfile)>>, // Ответ справочника, если получен.
    selected: HashSet<Uuid>,                 // Участники, которых выбрал владелец.
}

impl Picker {
    /// Готовит поле поиска для первого кадра диалога.
    fn new(conversation: &Conversation, cx: &mut Context<Conversation>) -> Self {
        let language = Preferences::read_for(cx, &conversation.address.route.owner).language;
        let input = cx.new(|cx| {
            TextInput::new(
                "details-member-picker-query",
                language.tr("desktop.conversation.members.findAMember"),
                false,
                cx,
            )
        });
        let change = cx.subscribe(&input, |_, _, _: &TextInputChanged, cx| cx.notify());
        Self {
            input,
            _change: change,
            request: None,
            users: None,
            selected: HashSet::new(),
        }
    }

    /// Рисует варианты выбора, статусы и общие действия формы.
    pub(super) fn content(
        &self,
        palette: Palette,
        window: &Window,
        cx: &Context<Conversation>,
        channel: &workspace_core::messenger::details::ChannelSidebar,
        panel: &Panel,
        view: &Conversation,
    ) -> Div {
        let pending = panel.mutation.is_some() || !matches!(panel.state, Some(State::Ready));
        let add_error = panel.mutation_error == Some(MutationKind::Add);
        let language = Preferences::read_for(cx, &view.address.route.owner).language;
        let text = |key| language.tr(key);
        let existing = channel
            .members
            .iter()
            .map(|member| member.user_id)
            .collect::<HashSet<_>>();
        let query = self.input.read(cx).text().trim().to_lowercase();
        let mut list = div()
            .id("details-member-picker-list")
            .flex()
            .flex_col()
            .gap_1()
            .flex_1()
            .min_h_0()
            .overflow_x_hidden()
            .overflow_y_scroll();
        if let Some(users) = &self.users {
            let filtered = users
                .iter()
                .filter(|(id, user)| {
                    !existing.contains(id)
                        && (user.display_name.to_lowercase().contains(&query)
                            || user
                                .email
                                .as_ref()
                                .is_some_and(|email| email.to_lowercase().contains(&query)))
                })
                .collect::<Vec<_>>();
            if filtered.is_empty() {
                list = list.child(
                    label(
                        palette,
                        TextRole::Body,
                        text("channel.noMembers"),
                    )
                    .py_2(),
                );
            }
            for (id, user) in filtered {
                let chosen = self.selected.contains(id);
                let selector = gpui::SharedString::from(format!("details-member-picker-user-{id}"));
                let mut avatar = Avatar::from_initials(
                    user.display_name
                        .chars()
                        .next()
                        .map(|letter| letter.to_uppercase().collect::<String>())
                        .unwrap_or_default(),
                );
                avatar.presence = match user.presence {
                    Some(workspace_core::directory::ProfilePresence::Active) => {
                        AvatarPresence::Online
                    }
                    Some(workspace_core::directory::ProfilePresence::Idle) => AvatarPresence::Away,
                    _ => AvatarPresence::Hidden,
                };
                let row = div()
                    .id(selector.clone())
                    .debug_selector(move || selector.to_string())
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(gpui::transparent_black())
                    .when(chosen, |row| row.bg(palette.card_selected))
                    .child(avatar.render_at(palette, px(32.), palette.surface))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .child(
                                label(palette, TextRole::Body, user.display_name.clone())
                                    .truncate(),
                            )
                            .children(user.email.iter().map(|email| {
                                label(palette, TextRole::Caption, email.clone()).truncate()
                            })),
                    )
                    .children(chosen.then(|| Icon::Check.element(px(16.), palette.accent)));
                let id = *id;
                list = list.child(if pending {
                    row
                } else {
                    activate(
                        row.hover(|style| style.bg(palette.card_hover)),
                        None,
                        palette,
                        action(cx, move |view, _, cx| view.toggle_picked_member(id, cx)),
                    )
                });
            }
        } else if self.request.is_some() {
            list = list.child(
                label(
                    palette,
                    TextRole::Body,
                    text("desktop.conversation.members.loadingMembers"),
                )
                .py_2(),
            );
        } else {
            list = list
                .child(
                    label(
                        palette,
                        TextRole::Body,
                        text("desktop.conversation.members.couldNotLoadTheDirectory"),
                    )
                    .py_2(),
                )
                .child(
                    Button::new("details-member-picker-retry", text("common.retry")).render(
                        palette,
                        None,
                        action(cx, |view, _, cx| view.request_member_directory(cx)),
                    ),
                );
        }
        let mut submit = Button::new("details-member-picker-submit", text("common.add"));
        submit.appearance = ButtonAppearance::Primary;
        submit.disabled = pending || self.selected.iter().all(|id| existing.contains(id));
        let mut close = Button::new("details-member-picker-close", "");
        close.icon = Some(Icon::Close);
        close.appearance = ButtonAppearance::Ghost;
        let viewport = window.viewport_size();
        div()
            .debug_selector(|| "details-member-picker-dialog".into())
            .w((viewport.width - px(24.)).clamp(px(0.), px(448.)))
            .h(viewport.height * 0.7)
            .max_h(px(520.))
            .rounded(SURFACE_RADIUS)
            .bg(palette.surface)
            .border_1()
            .border_color(palette.border)
            .flex()
            .flex_col()
            .min_h_0()
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .border_b_1()
                    .border_color(palette.border)
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(label(palette, TextRole::Label, text("channel.addMembers")))
                            .child(
                                label(palette, TextRole::Caption, channel.name.clone()).truncate(),
                            ),
                    )
                    .px_4()
                    .py_3()
                    .child(
                        close
                            .render(palette, None, action(cx, Conversation::close_member_picker))
                            .tooltip(move |_, app| {
                                crate::gpui_client::ui::popup::tooltip(
                                    palette,
                                    language.tr("common.close").into(),
                                    app,
                                )
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .gap_2()
                    .p_4()
                    .child(self.input.clone())
                    .child(list)
                    .when(add_error, |group| {
                        group.child(
                            label(
                                palette,
                                TextRole::Caption,
                                text("desktop.conversation.members.couldNotAddMembers"),
                            )
                            .text_color(palette.danger),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new(
                                    "details-member-picker-cancel",
                                    text("common.cancel"),
                                )
                                .render(
                                    palette,
                                    None,
                                    action(cx, Conversation::close_member_picker),
                                ),
                            )
                            .child(
                                submit
                                    .render(
                                        palette,
                                        None,
                                        action(cx, Conversation::submit_picked_members),
                                    )
                                    .debug_selector(|| "details-member-picker-submit".into()),
                            ),
                    ),
            )
    }
}

/// Показывает выбор участников поверх окна и следит за живым разговором.
pub(super) struct PickerDialog {
    host: WeakEntity<RightPanel>, // Оверлей окна, который вернёт фокус.
    conversation: WeakEntity<Conversation>, // Разговор, который проверяет права и отправляет команду.
    first_frame: bool,                      // Поле получает фокус после размещения диалога.
    _changes: Subscription, // Обновляет диалог при получении списка или изменении прав.
}

impl Render for PickerDialog {
    /// Показывает форму только пока актуальны разговор и разрешение на приглашение.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_window(window, cx);
        let first = std::mem::take(&mut self.first_frame);
        let content = self
            .conversation
            .update(cx, |view, cx| {
                let panel = view.details.as_ref()?;
                let (Some(picker), Some(SidebarDetails::Channel(channel))) =
                    (&panel.picker, panel.snapshot.as_deref())
                else {
                    return None;
                };
                if first {
                    picker.input.read(cx).focus(window);
                }
                Some(picker.content(palette, window, cx, channel, panel, view))
            })
            .ok()
            .flatten();
        if content.is_none() {
            let _ = self.host.update(cx, |host, cx| {
                if host.modal_is::<Self>() {
                    host.close_modal(window, cx);
                }
            });
        }
        content.unwrap_or_else(div)
    }
}

impl Conversation {
    /// Открывает выбор только для владельца с готовой панелью.
    pub(super) fn open_member_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = &self.details else {
            return;
        };
        if panel.picker.is_some()
            || panel.mutation.is_some()
            || !matches!(panel.state, Some(State::Ready))
        {
            return;
        }
        let conversation = cx.entity();
        let host = panel.host.clone();
        let opener = panel.add_members_focus.clone();
        let opened = host.update(cx, |host, cx| {
            let weak_host = cx.entity().downgrade();
            host.open_conversation_modal(&self.address, opener, window, cx, |_, cx| {
                cx.new(|cx| PickerDialog {
                    host: weak_host,
                    conversation: conversation.downgrade(),
                    first_frame: true,
                    _changes: cx.observe(&conversation, |_, _, cx| cx.notify()),
                })
            })
        });
        if opened {
            let picker = Picker::new(self, cx);
            let panel = self.details.as_mut().expect("Открыты сведения канала");
            panel.mutation_error = None;
            panel.picker = Some(picker);
            self.request_member_directory(cx);
        }
    }

    /// Закрывает только приглашение и возвращает фокус на кнопку участников.
    pub(super) fn close_member_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = &mut self.details {
            panel.picker = None;
            panel.host.update(cx, |host, cx| {
                if host.modal_is::<PickerDialog>() {
                    host.close_modal(window, cx);
                }
            });
        }
    }

    /// Загружает список проекта и применяет результат только к тому же выбору.
    fn request_member_directory(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.details else {
            return;
        };
        let Some(picker) = &mut panel.picker else {
            return;
        };
        let id = Uuid::new_v4();
        let runtime = panel.runtime.clone();
        let address = self.address.clone();
        let task = cx.spawn(async move |view, cx| {
            let reply = source::directory(runtime, address).await;
            let _ = view.update(cx, |view, cx| {
                let Some(picker) = view
                    .details
                    .as_mut()
                    .and_then(|panel| panel.picker.as_mut())
                else {
                    return;
                };
                if !picker
                    .request
                    .as_ref()
                    .is_some_and(|(current, _)| *current == id)
                {
                    return;
                }
                let Some(result) = reply.for_current(Some(&view.address)) else {
                    return;
                };
                picker.request = None;
                if let Ok(mut users) = result {
                    users.sort_by_cached_key(|(_, user)| user.display_name.to_lowercase());
                    picker.users = Some(users);
                }
                cx.notify();
            });
        });
        picker.request = Some((id, task));
        cx.notify();
    }

    /// Переключает выбранного пользователя, пока отправка не началась.
    fn toggle_picked_member(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let Some(picker) = self
            .details
            .as_mut()
            .filter(|panel| panel.mutation.is_none())
            .and_then(|panel| panel.picker.as_mut())
        else {
            return;
        };
        if !picker.selected.remove(&id) {
            picker.selected.insert(id);
        }
        cx.notify();
    }

    /// Повторно исключает новых участников, которые появились после открытия списка.
    fn submit_picked_members(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = &self.details else {
            return;
        };
        let (Some(picker), Some(SidebarDetails::Channel(channel))) =
            (&panel.picker, panel.snapshot.as_deref())
        else {
            return;
        };
        if !channel.can_add_members {
            return;
        }
        let selected: Vec<_> = picker
            .selected
            .iter()
            .copied()
            .filter(|id| !channel.members.iter().any(|member| member.user_id == *id))
            .collect();
        if !selected.is_empty() {
            self.start_sidebar_mutation(Mutation::AddMembers(selected), window, cx);
        }
    }
}
