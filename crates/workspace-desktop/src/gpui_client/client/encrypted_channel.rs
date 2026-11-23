//! Форма создания защищённого канала. Core сохраняет намерение и подтверждает серверное размещение.

use std::collections::HashSet;

use gpui::{
    Context, Entity, IntoElement, Render, Subscription, Task, WeakEntity, Window, div, prelude::*,
    px,
};
use uuid::Uuid;
use workspace_core::{
    directory::UserProfile,
    messenger::encryption::{
        EncryptionActivation,
        lifecycle::{
            CreatedEncryptedChannel, EncryptedChannelInput, KeyLifecycleError, KeyLifecycleIntent,
        },
    },
};

use super::Client;
use crate::gpui_client::{
    contracts::AccountSession,
    preferences::Preferences,
    right_panel::RightPanel,
    runtime::RuntimeClient,
    sidebar::OpenEncryptedChannel,
    ui::{Button, ButtonAppearance, Palette, TextInput, TextInputChanged, TextRole, label},
};

impl Client {
    /// Открывает форму в штатной рамке только для выбранного действующего входа.
    pub(super) fn open_requested_encrypted_channel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(intent) = self.encrypted_channel.take() else {
            return;
        };
        if self.shell.selected() != Some(&intent.session) || self.sign_in.is_some() {
            return;
        }
        let Some((_, activation)) = self.runtime.message_encryption(&intent.session) else {
            return;
        };
        let host = self.right_panel.downgrade();
        let root = cx.entity().downgrade();
        let session = intent.session.clone();
        let runtime = self.runtime.clone();
        self.right_panel.update(cx, |panel, cx| {
            let mut active = None;
            panel.open_modal(&intent.session, intent.opener, window, cx, |_, cx| {
                let view = cx.new(|cx| {
                    CreateEncryptedChannel::new(root, host, session, activation, runtime, cx)
                });
                active = Some(view.downgrade());
                view
            });
            if let Some(view) = active {
                panel.set_modal_dismiss(move |app| {
                    view.upgrade()
                        .is_none_or(|view| view.read(app).saving.is_none())
                });
            }
        });
    }

    /// Откладывает открытие до кадра, когда доступна общая рамка и системное окно.
    pub(super) fn request_encrypted_channel(
        &mut self,
        intent: &OpenEncryptedChannel,
        cx: &mut Context<Self>,
    ) {
        if self.shell.selected() == Some(&intent.session) {
            self.encrypted_channel = Some(intent.clone());
            cx.notify();
        }
    }

    /// Открывает подтверждённую тему и обновляет каталог того же аккаунта.
    fn created_encrypted_channel(
        &mut self,
        session: &AccountSession,
        created: CreatedEncryptedChannel,
        cx: &mut Context<Self>,
    ) {
        let effect =
            self.shell
                .open_created_encrypted_channel(session, created.stream_id, created.topic_id);
        if effect.is_some() {
            let runtime = self.runtime.clone();
            let session = session.clone();
            cx.spawn(async move |_, _| {
                let _ = runtime.retry_catalog(session).await;
            })
            .detach();
            self.apply(effect, cx);
        }
    }
}

/// Состояние одной открытой формы. Секреты и подготовленные команды принадлежат Core.
struct CreateEncryptedChannel {
    root: WeakEntity<Client>,     // Рабочее окно для перехода к созданной теме.
    host: WeakEntity<RightPanel>, // Системная рамка и возврат фокуса.
    session: AccountSession,      // Выбранный вход, которому принадлежит новое намерение.
    activation: EncryptionActivation, // Точный выбор, захваченный при открытии формы.
    runtime: RuntimeClient,       // Фоновый вызов сервера и защищённого хранилища.
    name: Entity<TextInput>,      // Поле названия, пока операция не подготовлена.
    description: Entity<TextInput>, // Необязательное описание канала.
    search: Entity<TextInput>,    // Поиск участников в полученном справочнике.
    people: Option<Vec<(Uuid, UserProfile)>>, // None, если справочник пока недоступен.
    selected: HashSet<Uuid>,      // Явно выбранные участники в черновике формы.
    loaded: bool,                 // Core подтвердил список незавершённых операций.
    failed: bool,                 // Последнее действие не получило подтверждения.
    resume: Option<Uuid>,         // Прежнее намерение, которое только сверяем или повторяем.
    saving: Option<Task<()>>,     // Текущий запуск; закрывать форму можно после его результата.
    first_frame: bool,            // Начальный фокус поля.
    _changes: [Subscription; 3],  // Ввод и поиск инвалидируют только форму.
}

impl CreateEncryptedChannel {
    /// Показывает пустую форму и проверяет, не остался ли незавершённый запрос устройства.
    fn new(
        root: WeakEntity<Client>,
        host: WeakEntity<RightPanel>,
        session: AccountSession,
        activation: EncryptionActivation,
        runtime: RuntimeClient,
        cx: &mut Context<Self>,
    ) -> Self {
        let locale = Preferences::read(cx).language;
        let name = cx.new(|cx| {
            TextInput::new(
                "e2ee-channel-name",
                locale.text("Название канала", "Channel name"),
                false,
                cx,
            )
        });
        let description = cx.new(|cx| {
            TextInput::new(
                "e2ee-channel-description",
                locale.text("Описание", "Description"),
                false,
                cx,
            )
        });
        let search = cx.new(|cx| {
            TextInput::new(
                "e2ee-channel-members-search",
                locale.text("Найти участника", "Find a member"),
                false,
                cx,
            )
        });
        let changes = [&name, &description, &search]
            .map(|input| cx.subscribe(input, |_, _, _: &TextInputChanged, cx| cx.notify()));
        let mut view = Self {
            root,
            host,
            session,
            activation,
            runtime,
            name,
            description,
            search,
            people: None,
            selected: HashSet::new(),
            loaded: false,
            failed: false,
            resume: None,
            saving: None,
            first_frame: true,
            _changes: changes,
        };
        view.load(cx);
        view
    }

    /// Возвращает исходные параметры подготовленного создания и доступных участников.
    fn load(&mut self, cx: &mut Context<Self>) {
        self.failed = false;
        let runtime = self.runtime.clone();
        let selected = self.activation.clone();
        let session = self.session.clone();
        cx.spawn(async move |view, cx| {
            let result = runtime
                .request(move |services| async move {
                    let pending = services
                        .messenger
                        .pending_key_lifecycle(&services.sync, &selected)?;
                    let saved = pending
                        .into_iter()
                        .find_map(|operation| match operation.intent {
                            KeyLifecycleIntent::CreateChannel(input) => {
                                Some((operation.operation_id, input))
                            }
                            KeyLifecycleIntent::Rotate { .. } => None,
                        });
                    let people = services
                        .messenger
                        .sidebar_directory(&services.sync, &session.owner, session.generation)
                        .await
                        .ok();
                    Ok::<_, workspace_core::messenger::encryption::lifecycle::KeyLifecycleError>((
                        saved, people,
                    ))
                })
                .await;
            let _ = view.update(cx, |view, cx| {
                match result {
                    Ok(Ok((saved, people))) => {
                        view.people = people;
                        view.loaded = true;
                        if let Some((op, input)) = saved {
                            view.resume = Some(op);
                            view.name
                                .update(cx, |field, cx| field.set_text(&input.name, cx));
                            view.description
                                .update(cx, |field, cx| field.set_text(&input.description, cx));
                            view.selected = input.members.into_iter().collect();
                        }
                        view.update_fields(cx);
                    }
                    _ => view.failed = true,
                }
                cx.notify();
            });
        })
        .detach();
        self.update_fields(cx);
    }

    /// Поля доступны, пока у формы нет подтверждённого намерения для повтора.
    fn update_fields(&self, cx: &mut Context<Self>) {
        let enabled = self.loaded && self.resume.is_none() && self.saving.is_none();
        for field in [&self.name, &self.description, &self.search] {
            field.update(cx, |field, cx| field.set_enabled(enabled, cx));
        }
    }

    /// Готовит новое намерение либо продолжает прежнее с тем же точным идентификатором.
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name.read(cx).text().trim();
        if self.saving.is_some()
            || !self.loaded
            || name.is_empty()
            || self.name.read(cx).is_composing()
        {
            return;
        }
        let mut members: Vec<_> = self.selected.iter().copied().collect();
        members.sort_unstable();
        let input = EncryptedChannelInput {
            name: name.to_owned(),
            description: self.description.read(cx).text().trim().to_owned(),
            members,
        };
        let runtime = self.runtime.clone();
        let activation = self.activation.clone();
        let resume = self.resume;
        self.failed = false;
        self.saving = Some(cx.spawn_in(window, async move |view, cx| {
            let result = runtime
                .request(move |services| async move {
                    let operation = match resume {
                        Some(operation) => Ok(operation),
                        None => services.messenger.stage_encrypted_channel(
                            &services.sync,
                            &activation,
                            input,
                        ),
                    };
                    match operation {
                        Ok(id) => (
                            Some(id),
                            services
                                .messenger
                                .create_encrypted_channel(&services.sync, &activation, id)
                                .await,
                        ),
                        Err(error) => (None, Err(error)),
                    }
                })
                .await;
            let _ = cx.update(|window, app| {
                let _ = view.update(app, |view, cx| {
                    view.saving = None;
                    match result {
                        Ok((_, Ok(created))) => {
                            let _ = view.root.update(cx, |client, cx| {
                                client.created_encrypted_channel(&view.session, created, cx)
                            });
                            let _ = view
                                .host
                                .update(cx, |panel, cx| panel.close_modal(window, cx));
                        }
                        Ok((_, Err(KeyLifecycleError::CreateRejected(_)))) => {
                            view.resume = None;
                            view.failed = true;
                            view.update_fields(cx);
                        }
                        result => {
                            if let Ok((Some(id), _)) = result {
                                view.resume = Some(id);
                            }
                            view.failed = true;
                            view.update_fields(cx);
                        }
                    }
                    cx.notify();
                });
            });
        }));
        self.update_fields(cx);
        cx.notify();
    }
}

impl Render for CreateEncryptedChannel {
    /// Показывает название, описание, выбор людей и состояние фоновой операции.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_window(window, cx);
        let locale = Preferences::read(cx).language;
        let t = |ru, en| locale.text(ru, en);
        if self.loaded && std::mem::take(&mut self.first_frame) && self.resume.is_none() {
            self.name.read(cx).focus(window);
        }
        let field = |entity: &Entity<TextInput>| div().w_full().child(entity.clone());
        let target = cx.entity().downgrade();
        let mut submit = Button::new(
            "e2ee-channel-submit",
            if self.resume.is_some() {
                t("Продолжить создание", "Resume creation")
            } else {
                t("Создать канал", "Create channel")
            },
        );
        submit.appearance = ButtonAppearance::Primary;
        submit.disabled =
            self.saving.is_some() || !self.loaded || self.name.read(cx).text().trim().is_empty();
        let mut cancel = Button::new("e2ee-channel-cancel", t("Закрыть", "Close"));
        cancel.disabled = self.saving.is_some();
        let host = self.host.clone();
        let retry = cx.entity().downgrade();
        let query = self.search.read(cx).text().trim().to_lowercase();
        let mut people = div()
            .id("e2ee-channel-members")
            .max_h(px(180.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_1();
        if let Some(available) = &self.people {
            for (id, person) in available.iter().filter(|(id, person)| {
                let selected = self.selected.contains(id);
                (self.resume.is_none() || selected)
                    && self.session.owner.user_id.parse::<Uuid>().ok() != Some(*id)
                    && (query.is_empty() || person.display_name.to_lowercase().contains(&query))
            }) {
                let id = *id;
                let selected = self.selected.contains(&id);
                let mut button = Button::new(
                    (gpui::ElementId::from(id), "invite-e2ee-member"),
                    person.display_name.clone(),
                );
                button.appearance = if selected {
                    ButtonAppearance::Primary
                } else {
                    ButtonAppearance::Neutral
                };
                button.disabled = self.resume.is_some() || self.saving.is_some();
                let target = cx.entity().downgrade();
                people = people.child(
                    button
                        .render(palette, None, move |_, app| {
                            let _ = target.update(app, |view, cx| {
                                if !view.selected.insert(id) {
                                    view.selected.remove(&id);
                                }
                                cx.notify();
                            });
                        })
                        .debug_selector(move || format!("e2ee-invite-member-{id}")),
                );
            }
        }
        let resume = self.resume.is_some();
        div()
            .w(px(440.))
            .max_w_full()
            .p_4()
            .rounded(px(12.))
            .bg(palette.surface)
            .flex()
            .flex_col()
            .gap_2()
            .debug_selector(|| "e2ee-create-channel-dialog".into())
            .child(label(
                palette,
                TextRole::Heading,
                t("Защищённый канал", "Encrypted channel"),
            ))
            .child(field(&self.name))
            .child(field(&self.description))
            .children(self.people.as_ref().map(|_| {
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(label(palette, TextRole::Caption, t("Участники", "Members")))
                    .child(field(&self.search))
                    .child(people)
            }))
            .children(self.failed.then(|| {
                label(
                    palette,
                    TextRole::Caption,
                    t(
                        "Не удалось подтвердить создание",
                        "Could not confirm channel creation",
                    ),
                )
                .debug_selector(|| "e2ee-channel-failure".into())
            }))
            .children((self.failed && !self.loaded).then(|| {
                Button::new("e2ee-channel-reload", t("Повторить", "Retry")).render(
                    palette,
                    None,
                    move |_, app| {
                        let _ = retry.update(app, |view, cx| view.load(cx));
                    },
                )
            }))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(cancel.render(palette, None, move |window, app| {
                        let _ = host.update(app, |panel, cx| panel.close_modal(window, cx));
                    }))
                    .child(
                        submit
                            .render(palette, None, move |window, app| {
                                let _ = target.update(app, |view, cx| view.submit(window, cx));
                            })
                            .debug_selector(move || {
                                if resume {
                                    "e2ee-channel-resume".into()
                                } else {
                                    "e2ee-channel-submit".into()
                                }
                            }),
                    ),
            )
    }
}
