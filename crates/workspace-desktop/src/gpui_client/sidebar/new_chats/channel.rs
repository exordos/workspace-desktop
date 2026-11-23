//! Собирает две последовательные формы канала и повторяет приглашение по уже созданному UUID.

use gpui::{AnyElement, Context, Entity, FocusHandle, Render, WeakEntity, Window, div, prelude::*};
use uuid::Uuid;
use workspace_core::{
    messenger::creation::{ChannelCreation, ConversationCreationError, CreatedChannel},
    storage::HistoryScope,
};

use crate::gpui_client::{
    contracts::AccountSession,
    preferences::{Language, Preferences},
    runtime::{RequestError, RuntimeClient},
    ui::{
        Button, ButtonAppearance, Palette, TextInput, TextInputChanged, TextInputSubmitted,
        TextRole, dialog_card, label,
    },
};

use super::{Load, NewChats, people::People};

/// Причина ошибки, которую форма может объяснить без деталей запроса.
#[derive(Clone, Copy)]
enum Problem {
    InvalidInput, // Название или описание превышает предел API.
    Create,       // Пользователь может исправить ввод или повторить создание.
    Unconfirmed,  // Повтор должен использовать те же поля и UUID попытки.
    AddMembers,   // Канал готов, повтор относится только к приглашению.
}

/// Переиспользует карточку диалога, поля и подборщик людей в двух этапах.
pub(in crate::gpui_client) struct ChannelDialog {
    runtime: RuntimeClient,           // Фоновый поток Core.
    session: AccountSession,          // Вход, в котором пользователь начал создание.
    revision: u64,                    // Версия открытия панели для проверки позднего ответа.
    origin: WeakEntity<NewChats>,     // Панель, которая открывает подтверждённый канал.
    name: Entity<TextInput>,          // Обязательное название.
    description: Entity<TextInput>,   // Необязательное описание.
    people: People,                   // Поиск и множественный выбор коллег.
    attempt: Option<ChannelCreation>, // Тот же UUID используется при повторе создания.
    created: Option<CreatedChannel>,  // Some после создания, до добавления участников.
    problem: Option<Problem>,         // Ошибка текущего шага, если она есть.
    busy: bool,                       // Сейчас выполняется изменение на сервере.
    focus_input: bool,                // На следующем кадре сфокусировать поле текущего шага.
    primary: FocusHandle,             // Фокус основной кнопки модалки.
    secondary: FocusHandle,           // Фокус отмены или пропуска.
    retry_people: FocusHandle,        // Фокус повтора загрузки списка участников.
    region: FocusHandle,              // Резервный фокус общего модального хоста.
}

impl ChannelDialog {
    /// Готовит поля и поведение Enter для рабочего окна.
    pub(in crate::gpui_client) fn new(
        runtime: RuntimeClient,
        session: AccountSession,
        revision: u64,
        origin: WeakEntity<NewChats>,
        region: FocusHandle,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| TextInput::new("new-channel-name", "", false, cx));
        let description = cx.new(|cx| TextInput::new("new-channel-description", "", false, cx));
        for field in [&name, &description] {
            cx.subscribe(field, |_, _, _: &TextInputChanged, cx| cx.notify())
                .detach();
            cx.subscribe(field, |dialog, _, _: &TextInputSubmitted, cx| {
                dialog.submit(cx);
            })
            .detach();
        }
        Self {
            runtime,
            session,
            revision,
            origin,
            name,
            description,
            people: People::new(cx),
            attempt: None,
            created: None,
            problem: None,
            busy: false,
            focus_input: true,
            primary: cx.focus_handle(),
            secondary: cx.focus_handle(),
            retry_people: cx.focus_handle(),
            region,
        }
    }

    /// Заполняет реальную модалку синтетическими полями для отдельного снимка.
    #[cfg(debug_assertions)]
    pub(super) fn preview_content(
        &mut self,
        members: bool,
        people: Vec<(Uuid, workspace_core::directory::UserProfile)>,
        cx: &mut Context<Self>,
    ) {
        self.focus_input = false;
        self.name
            .update(cx, |input, cx| input.set_text("Проектная команда", cx));
        if members {
            self.created = Some(CreatedChannel {
                stream_id: Uuid::from_u128(200),
                topic_id: Uuid::from_u128(201),
            });
            self.people
                .selected
                .extend(people.iter().take(2).map(|person| person.0));
            self.people.data = Load::Ready(people);
        }
    }

    /// Позволяет закрыть диалог по Escape и клику снаружи между запросами.
    pub(in crate::gpui_client) fn can_dismiss(&self) -> bool {
        !self.busy
    }

    /// Выполняет только действие текущего шага.
    fn submit(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        if let Some(channel) = self.created {
            self.invite(channel, cx);
        } else {
            self.create(cx);
        }
    }

    /// Создаёт канал, оставляя тот же UUID для повтора при неясном результате.
    fn create(&mut self, cx: &mut Context<Self>) {
        let name = self.name.read(cx).text();
        let description = self.description.read(cx).text();
        let attempt = match &self.attempt {
            Some(attempt) if attempt.matches(name, description) => attempt.clone(),
            _ => match ChannelCreation::new(name, description) {
                Ok(attempt) => attempt,
                Err(_) => {
                    self.problem = Some(Problem::InvalidInput);
                    cx.notify();
                    return;
                }
            },
        };
        self.attempt = Some(attempt.clone());
        let unresolved = matches!(self.problem, Some(Problem::Unconfirmed));
        self.problem = None;
        self.busy = true;
        self.focus_input = true;
        self.enable_fields(false, cx);
        let (runtime, session) = (self.runtime.clone(), self.session.clone());
        log::debug!(
            "gpui_new_channel op_id={} event=create_started",
            self.revision
        );
        cx.spawn(async move |entity, cx| {
            let result = runtime.new_chat_create_channel(session, attempt).await;
            let _ = entity.update(cx, |dialog, cx| {
                dialog.busy = false;
                match result {
                    Ok(Ok(channel)) => {
                        dialog.created = Some(channel);
                        dialog.focus_input = true;
                        dialog.load_people(cx);
                    }
                    Ok(Err(ConversationCreationError::Unknown)) | Err(RequestError::Stopped) => {
                        dialog.problem = Some(Problem::Unconfirmed);
                        dialog.focus_input = true;
                    }
                    _ if unresolved => {
                        dialog.problem = Some(Problem::Unconfirmed);
                        dialog.focus_input = true;
                    }
                    _ => {
                        dialog.problem = Some(Problem::Create);
                        dialog.enable_fields(true, cx);
                        dialog.focus_input = true;
                    }
                }
                log::debug!(
                    "gpui_new_channel op_id={} event=create_finished confirmed={}",
                    dialog.revision,
                    dialog.created.is_some()
                );
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Отключает редактирование на время запроса или повтора его исходного ввода.
    fn enable_fields(&self, enabled: bool, cx: &mut Context<Self>) {
        for field in [&self.name, &self.description] {
            field.update(cx, |input, cx| input.set_enabled(enabled, cx));
        }
    }

    /// Загружает коллег тем же путём, что и выбор нового личного разговора.
    fn load_people(&mut self, cx: &mut Context<Self>) {
        self.people.data = Load::Pending;
        let (runtime, session) = (self.runtime.clone(), self.session.clone());
        cx.spawn(async move |entity, cx| {
            let result = runtime.new_chat_people(session).await;
            let _ = entity.update(cx, |dialog, cx| {
                dialog.people.data = match result {
                    Ok(Ok(people)) => Load::Ready(people),
                    _ => Load::Failed,
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Переключает выбранного коллегу, сохраняя выбор при смене поиска.
    fn toggle_person(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if !self.busy {
            if !self.people.selected.remove(&id) {
                self.people.selected.insert(id);
            }
            cx.notify();
        }
    }

    /// Загружает фотографии только видимых строк списка.
    fn visible_people(&mut self, ids: Vec<Uuid>, cx: &mut Context<Self>) {
        self.people
            .set_visible(ids, &self.runtime, &self.session, cx);
    }

    /// Добавляет участников в подтверждённый канал, сохраняя выбор при ошибке.
    fn invite(&mut self, channel: CreatedChannel, cx: &mut Context<Self>) {
        let users: Vec<_> = self.people.selected.iter().copied().collect();
        if users.is_empty() {
            return;
        }
        self.problem = None;
        self.busy = true;
        self.focus_input = true;
        let (runtime, session) = (self.runtime.clone(), self.session.clone());
        log::debug!(
            "gpui_new_channel op_id={} event=members_started",
            self.revision
        );
        cx.spawn(async move |entity, cx| {
            let added = matches!(
                runtime
                    .new_chat_add_members(session, channel.stream_id, users)
                    .await,
                Ok(Ok(()))
            );
            let _ = entity.update(cx, |dialog, cx| {
                dialog.busy = false;
                log::debug!(
                    "gpui_new_channel op_id={} event=members_finished confirmed={added}",
                    dialog.revision
                );
                if added {
                    dialog.finish(cx);
                } else {
                    dialog.problem = Some(Problem::AddMembers);
                    dialog.focus_input = true;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// Закрывает форму или, после успешного создания, открывает начальную тему.
    fn finish(&mut self, cx: &mut Context<Self>) {
        let session = self.session.clone();
        let revision = self.revision;
        let scope = self.created.map(|channel| HistoryScope {
            stream_id: channel.stream_id,
            topic_id: Some(channel.topic_id),
        });
        let _ = self.origin.update(cx, |view, cx| {
            view.finish_channel(session, revision, scope, cx);
        });
    }

    /// Использует общую кнопку со штатным фокусом и устойчивым идентификатором.
    fn button(
        &self,
        id: &'static str,
        title: impl Into<gpui::SharedString>,
        style: (ButtonAppearance, bool, &FocusHandle),
        action: fn(&mut Self, &mut Context<Self>),
        palette: Palette,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let entity = cx.entity().downgrade();
        let mut button = Button::new(id, title);
        let (appearance, disabled, focus) = style;
        button.appearance = appearance;
        button.disabled = disabled;
        button
            .render(palette, Some(focus), move |_, app| {
                let _ = entity.update(app, action);
            })
            .debug_selector(|| id.into())
    }

    /// Показывает ошибку рядом с полями, сохраняя введённые данные и выбор.
    fn notice(&self, locale: Language, palette: Palette) -> Option<AnyElement> {
        let (id, text) = match self.problem? {
            Problem::InvalidInput => (
                "new-channel-error",
                locale.text(
                    "Проверьте длину названия и описания",
                    "Check the name and description length",
                ),
            ),
            Problem::Create => (
                "new-channel-error",
                locale.text("Не удалось создать канал", "Could not create channel"),
            ),
            Problem::Unconfirmed => (
                "new-channel-error",
                locale.text(
                    "Создание не подтверждено. Повторите попытку",
                    "Creation is unconfirmed. Try again",
                ),
            ),
            Problem::AddMembers => (
                "new-channel-members-error",
                locale.text("Не удалось добавить участников", "Could not add members"),
            ),
        };
        Some(
            label(palette, TextRole::Body, text)
                .debug_selector(|| id.into())
                .into_any_element(),
        )
    }
}

impl Render for ChannelDialog {
    /// Переключает содержимое общей карточки между созданием и добавлением людей.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let locale = Preferences::read(cx).language;
        let palette = Palette::for_window(window, cx);
        if std::mem::take(&mut self.focus_input) {
            if self.busy {
                window.focus(&self.region);
            } else if matches!(
                self.problem,
                Some(Problem::Unconfirmed | Problem::AddMembers)
            ) {
                window.focus(&self.primary);
            } else if self.created.is_some() {
                self.people.query.read(cx).focus(window);
            } else {
                self.name.read(cx).focus(window);
            }
        }

        let mut body = div().flex().flex_col().gap_2();
        let footer = div().flex().flex_wrap().justify_end().gap_2();
        let (id, title, footer) = if self.created.is_some() {
            body = body
                .child(self.people.search(palette, window, cx))
                .child(
                    self.people
                        .results(
                            palette,
                            self.busy,
                            cx,
                            Self::toggle_person,
                            Self::visible_people,
                        )
                        .max_h(gpui::px(458.)),
                )
                .children(self.people.data.failed().then(|| {
                    self.button(
                        "new-channel-members-retry",
                        locale.text("Повторить", "Retry"),
                        (ButtonAppearance::Neutral, false, &self.retry_people),
                        Self::load_people,
                        palette,
                        cx,
                    )
                }));
            let selected = self.people.selected.len();
            let action = if self.busy {
                locale.text("Добавляем…", "Adding…").into()
            } else if selected > 0 {
                format!("{} ({selected})", locale.text("Добавить", "Add"))
            } else {
                locale.text("Добавить", "Add").into()
            };
            let footer = footer
                .child(self.button(
                    "new-channel-add-members",
                    action,
                    (
                        ButtonAppearance::Primary,
                        self.busy || selected == 0,
                        &self.primary,
                    ),
                    Self::submit,
                    palette,
                    cx,
                ))
                .child(self.button(
                    "new-channel-skip-members",
                    locale.text("Пропустить", "Skip"),
                    (ButtonAppearance::Neutral, self.busy, &self.secondary),
                    Self::finish,
                    palette,
                    cx,
                ));
            (
                "new-channel-members-dialog",
                locale.text("Добавить участников", "Add members"),
                footer,
            )
        } else {
            body = body
                .child(label(
                    palette,
                    TextRole::Caption,
                    locale.text("Название канала", "Channel name"),
                ))
                .child(self.name.clone())
                .child(label(
                    palette,
                    TextRole::Caption,
                    locale.text("Описание (необязательно)", "Description (optional)"),
                ))
                .child(self.description.clone());
            let action = if self.busy {
                locale.text("Создаём…", "Creating…")
            } else if matches!(self.problem, Some(Problem::Unconfirmed)) {
                locale.text("Повторить", "Retry")
            } else {
                locale.text("Создать", "Create")
            };
            let footer = footer
                .child(self.button(
                    "new-channel-create",
                    action,
                    (
                        ButtonAppearance::Primary,
                        self.busy || self.name.read(cx).text().trim().is_empty(),
                        &self.primary,
                    ),
                    Self::submit,
                    palette,
                    cx,
                ))
                .child(self.button(
                    "new-channel-cancel",
                    locale.text("Отменить", "Cancel"),
                    (ButtonAppearance::Neutral, self.busy, &self.secondary),
                    Self::finish,
                    palette,
                    cx,
                ));
            (
                "new-channel-dialog",
                locale.text("Создать канал", "Create channel"),
                footer,
            )
        };
        dialog_card(
            id,
            title,
            body.children(self.notice(locale, palette)),
            footer,
            palette,
            window,
        )
        .debug_selector(|| id.into())
    }
}
