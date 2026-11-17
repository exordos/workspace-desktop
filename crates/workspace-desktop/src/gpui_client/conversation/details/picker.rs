//! Даёт владельцу канала поиск, выбор и добавление участников из справочника.

use std::collections::HashSet;

use gpui::{Context, Div, Entity, Subscription, Task, Window, div, prelude::*};
use uuid::Uuid;
use workspace_core::{directory::UserProfile, messenger::details::SidebarDetails};

use super::{Conversation, Mutation, State, action, source};
use crate::gpui_client::ui::{
    Button, ButtonAppearance, Icon, Palette, TextInput, TextInputChanged, TextRole, label,
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
    /// Готовит поле поиска и переводит на него фокус.
    fn new(window: &mut Window, cx: &mut Context<Conversation>) -> Self {
        let input = cx
            .new(|cx| TextInput::new("details-member-picker-query", "Найти участника", false, cx));
        input.read(cx).focus(window);
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
        cx: &Context<Conversation>,
        channel: &workspace_core::messenger::details::ChannelSidebar,
        pending: bool,
        add_error: bool,
    ) -> Div {
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
                list = list.child(label(palette, TextRole::Body, "Участники не найдены").py_2());
            }
            for (id, user) in filtered {
                let chosen = self.selected.contains(id);
                let mut button = Button::new(
                    gpui::SharedString::from(format!("details-member-picker-user-{id}")),
                    user.display_name.clone(),
                );
                button.appearance = if chosen {
                    ButtonAppearance::Primary
                } else {
                    ButtonAppearance::Ghost
                };
                button.icon = chosen.then_some(Icon::Check);
                button.disabled = pending;
                let id = *id;
                list = list.child(
                    button
                        .render(
                            palette,
                            None,
                            action(cx, move |view, _, cx| {
                                view.toggle_picked_member(id, cx);
                            }),
                        )
                        .justify_start(),
                );
            }
        } else if self.request.is_some() {
            list = list.child(label(palette, TextRole::Body, "Загружаем список…").py_2());
        } else {
            list = list
                .child(label(palette, TextRole::Body, "Не удалось загрузить список").py_2())
                .child(
                    Button::new("details-member-picker-retry", "Повторить").render(
                        palette,
                        None,
                        action(cx, |view, _, cx| view.request_member_directory(cx)),
                    ),
                );
        }
        let mut submit = Button::new("details-member-picker-submit", "Добавить");
        submit.appearance = ButtonAppearance::Primary;
        submit.disabled = pending || self.selected.iter().all(|id| existing.contains(id));
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap_2()
            .px_2()
            .child(label(palette, TextRole::Caption, channel.name.clone()).truncate())
            .child(self.input.clone())
            .child(list)
            .when(add_error, |group| {
                group.child(
                    label(palette, TextRole::Caption, "Не удалось добавить участников")
                        .text_color(palette.danger),
                )
            })
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("details-member-picker-cancel", "Отмена").render(
                            palette,
                            None,
                            action(cx, Conversation::dismiss_details),
                        ),
                    )
                    .child(submit.render(
                        palette,
                        None,
                        action(cx, |view, _, cx| {
                            view.submit_picked_members(cx);
                        }),
                    )),
            )
    }
}

impl Conversation {
    /// Открывает выбор только для владельца с готовой панелью.
    pub(super) fn open_member_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.details else {
            return;
        };
        if panel.picker.is_some()
            || panel.mutation.is_some()
            || !matches!(panel.state, Some(State::Ready))
        {
            return;
        }
        panel.mutation_error = None;
        panel.picker = Some(Picker::new(window, cx));
        self.request_member_directory(cx);
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
    fn submit_picked_members(&mut self, cx: &mut Context<Self>) {
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
            self.start_sidebar_mutation(Mutation::AddMembers(selected), cx);
        }
    }
}
