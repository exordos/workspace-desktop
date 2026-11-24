//! Общий поиск коллег и строки выбора для лички и добавления в канал.

use std::{collections::HashSet, rc::Rc};

use gpui::{Context, Div, Entity, ScrollHandle, Stateful, Window, div, point, prelude::*, px};
use uuid::Uuid;
use workspace_core::directory::{ProfilePresence, UserProfile};

use crate::gpui_client::{
    avatars::AvatarImages,
    contracts::AccountSession,
    preferences::Preferences,
    runtime::RuntimeClient,
    ui::{
        Avatar, AvatarPresence, Palette, PersonRow, TextInput, TextInputChanged, TextRole, label,
    },
};

use super::{Load, search};

/// Поиск, выбранные UUID и подписки на фото видимых коллег.
pub(super) struct People {
    pub query: Entity<TextInput>, // Поле, которое также принимает клавиатурный фокус.
    pub data: Load<Vec<(Uuid, UserProfile)>>, // Пользователи, полученные для этого входа.
    pub selected: HashSet<Uuid>,  // Выбор сохраняется при смене запроса.
    scroll: ScrollHandle,         // Позиция прокрутки списка результатов.
    avatars: AvatarImages,        // Фото только видимых пользователей.
    visible: Vec<Uuid>,           // UUID, на которые уже открыты подписки.
}

impl People {
    /// Готовит поле поиска и обновление результатов при изменении текста.
    pub(super) fn new<T: 'static>(cx: &mut Context<T>) -> Self {
        let locale = Preferences::read(cx).language;
        let query = cx.new(|cx| {
            TextInput::new(
                "new-chats-people-search",
                locale.tr("desktop.sidebar.newChats.searchPerson"),
                false,
                cx,
            )
            .unframed()
        });
        let scroll = ScrollHandle::new();
        let scroll_on_query = scroll.clone();
        cx.subscribe(&query, move |_, _, _: &TextInputChanged, cx| {
            scroll_on_query.set_offset(point(px(0.), px(0.)));
            cx.notify();
        })
        .detach();
        Self {
            query,
            data: Load::Idle,
            selected: HashSet::new(),
            scroll,
            avatars: AvatarImages::default(),
            visible: Vec::new(),
        }
    }

    /// Отпускает фото и состояние закрытого или сменившегося поиска.
    pub(super) fn reset(&mut self, cx: &mut gpui::App) {
        self.query.update(cx, TextInput::clear);
        self.data = Load::Idle;
        self.selected.clear();
        self.avatars.clear();
        self.visible.clear();
    }

    /// Показывает общий поиск с иконкой и очисткой запроса.
    pub(super) fn search(&self, palette: Palette, window: &Window, app: &gpui::App) -> Div {
        search::render(&self.query, palette, window, app)
    }

    /// Открывает подписки на фото только для фактически показанных строк.
    pub(super) fn set_visible<T: 'static>(
        &mut self,
        users: Vec<Uuid>,
        runtime: &RuntimeClient,
        session: &AccountSession,
        cx: &mut Context<T>,
    ) {
        if self.visible != users {
            self.avatars
                .refresh(users.iter().copied(), runtime, session, cx);
            self.visible = users;
            cx.notify();
        }
    }

    /// Рисует подходящих пользователей и ограничивает фото видимой областью.
    pub(super) fn results<T: 'static>(
        &self,
        palette: Palette,
        pending: bool,
        cx: &Context<T>,
        pick: fn(&mut T, Uuid, &mut Context<T>),
        visible: fn(&mut T, Vec<Uuid>, &mut Context<T>),
    ) -> Stateful<Div> {
        let locale = Preferences::read(cx).language;
        let result = div().flex().flex_col().gap_1();
        let scroll = |rows: Div| {
            rows.id("new-chats-people-results")
                .min_h_0()
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
        };
        let Load::Ready(people) = &self.data else {
            let text = if self.data.failed() {
                locale.tr("desktop.sidebar.newChats.peopleFailed")
            } else {
                locale.tr("app.loading")
            };
            return scroll(result.child(label(palette, TextRole::Body, text).py_2()));
        };
        let query = self.query.read(cx).text().trim().to_lowercase();
        let people = people.iter().filter(|(_, profile)| {
            profile.display_name.to_lowercase().contains(&query)
                || profile
                    .email
                    .as_ref()
                    .is_some_and(|email| email.to_lowercase().contains(&query))
        });
        let target = cx.entity().downgrade();
        let mut ids = Vec::new();
        let mut rows = result;
        for (id, person) in people {
            let id = *id;
            ids.push(id);
            let name = &person.display_name;
            let mut avatar = Avatar::from_initials(initials(name));
            avatar.image = self.avatars.image(id, cx);
            avatar.presence = match person.presence {
                Some(ProfilePresence::Active) => AvatarPresence::Online,
                Some(ProfilePresence::Idle | ProfilePresence::DoNotDisturb) => AvatarPresence::Away,
                _ => AvatarPresence::Hidden,
            };
            let row = PersonRow {
                avatar,
                name: name.clone().into(),
                secondary: person
                    .status_label()
                    .or_else(|| person.email.clone())
                    .map(Into::into),
                role: None,
                selected: self.selected.contains(&id),
            };
            let selector = gpui::SharedString::from(format!("new-chats-person-{id}"));
            if pending {
                rows = rows.child(
                    row.render(palette)
                        .debug_selector(move || selector.to_string()),
                );
            } else {
                let target = target.clone();
                rows = rows.child(
                    row.render_action(selector.clone(), palette, None, move |_, app| {
                        let _ = target.update(app, |view, cx| pick(view, id, cx));
                    })
                    .debug_selector(move || selector.to_string()),
                );
            }
        }
        if ids.is_empty() {
            rows = rows.child(
                label(
                    palette,
                    TextRole::Body,
                    locale.tr("desktop.sidebar.newChats.peopleEmpty"),
                )
                .py_2(),
            );
        }
        let ids = Rc::new(ids);
        let viewport = self.scroll.clone();
        let previous = self.visible.clone();
        scroll(rows.on_children_prepainted(move |_, _, app| {
            let start = viewport.top_item();
            let end = viewport.bottom_item().saturating_add(1).min(ids.len());
            let users = ids.get(start..end).unwrap_or_default();
            if users != previous {
                let users = users.to_vec();
                let target = target.clone();
                app.defer(move |app| {
                    let _ = target.update(app, |view, cx| visible(view, users, cx));
                });
            }
        }))
    }
}

/// Собирает первые буквы имени и фамилии для запасного аватара.
pub(super) fn initials(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|word| word.chars().next())
        .take(2)
        .flat_map(char::to_uppercase)
        .collect()
}
