//! Показывает свои данные и сохраняет правки IAM и аватара из личной карточки.

use std::{sync::Arc, time::Duration};

use unicode_segmentation::UnicodeSegmentation;

use super::super::Client;
use gpui::{
    App, ClipboardItem, Context, Div, Entity, FocusHandle, Stateful, Task, WeakEntity, Window, div,
    prelude::*, px,
};
use workspace_core::{
    directory::{ProfilePresence, UserProfile},
    messenger::{
        details::sidebar_profile_link,
        own_profile::{PersonalEdit, PersonalIdentity},
    },
};

use crate::gpui_client::{
    avatars::AvatarImages,
    composer::Editor,
    contracts::AccountSession,
    preferences::{Language, Preferences},
    right_panel::RightPanel,
    runtime::RuntimeClient,
    ui::{
        Avatar, AvatarPresence, Button, ButtonAppearance, Icon, Palette, TextInput,
        TextInputChanged, TextRole, activate, ellipsized_label, label, profile_joined_date,
    },
};

#[path = "avatar.rs"]
mod avatar;
use avatar::{AvatarChoice, AvatarDialog};

/// Данные и переходы страницы своего профиля.
pub(super) struct Personal {
    host: WeakEntity<RightPanel>,       // Показывает диалог изменения фото.
    client: WeakEntity<Client>,         // Показывает подтверждённое имя в шапке.
    session: AccountSession,            // Владелец и поколение личной карточки.
    runtime: RuntimeClient,             // Фоновый вызов IAM и Workspace.
    profile: Option<UserProfile>,       // Публичные поля Workspace.
    identity: Option<PersonalIdentity>, // Редактируемые данные IAM, если сервер их вернул.
    avatar: AvatarImages,               // Общая подписка на фото, которое уже знает шапка.
    fields: [Entity<TextInput>; 3],     // Имя, фамилия и телефон.
    description: Entity<Editor>,        // Многострочное поле «О себе».
    caption: String,                    // Временное имя до ответа справочника.
    editing: bool,                      // Показывать поля правки вместо обычных действий.
    loading: bool,                      // Уже читаются IAM и публичные поля.
    edit_requested: bool,               // Открыть редактор сразу после догрузки IAM.
    waiting: Option<Task<()>>,          // Запрос сохранения, допускается только один.
    photo: Option<AvatarChoice>,        // Локальное изменение аватара до подтверждения.
    photo_focus: FocusHandle,           // Кнопка открытия диалога аватара.
    notice: Option<&'static str>,       // Ключ ошибки или предупреждения на экране.
    share_feedback: Option<Task<()>>,   // Возвращает исходную плитку после краткой подсказки.
    share_copied: bool,                 // После копирования ссылка ожидает вставки.
}

impl Personal {
    /// Создаёт карточку и независимо загружает сведения из IAM и справочника.
    pub(super) fn new(
        host: WeakEntity<RightPanel>,
        session: AccountSession,
        runtime: RuntimeClient,
        caption: String,
        client: WeakEntity<Client>,
        cx: &mut Context<Self>,
    ) -> Self {
        let fields = ["profile-first-name", "profile-last-name", "profile-phone"].map(|id| {
            let input = cx.new(|cx| TextInput::new(id, "", false, cx));
            cx.subscribe(&input, |_, _, _: &TextInputChanged, cx| cx.notify())
                .detach();
            input
        });
        let description = cx.new(Editor::for_conversation);
        let mut avatar = AvatarImages::default();
        avatar.refresh(session.user_uuid(), &runtime, &session, cx);
        let mut page = Self {
            host,
            client,
            session,
            runtime,
            profile: None,
            identity: None,
            avatar,
            fields,
            description,
            caption,
            editing: false,
            loading: false,
            edit_requested: false,
            waiting: None,
            photo: None,
            photo_focus: cx.focus_handle(),
            notice: None,
            share_copied: false,
            share_feedback: None,
        };
        page.load(cx);
        page
    }

    /// Загружает данные текущего владельца. Закрытая страница игнорирует ответ.
    fn load(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        self.loading = true;
        let runtime = self.runtime.clone();
        let session = self.session.clone();
        cx.spawn(async move |page, cx| {
            let result = runtime
                .request(move |services| async move {
                    futures::join!(
                        services.messenger.own_profile(
                            &services.sync,
                            &session.owner,
                            session.generation
                        ),
                        services.messenger.own_identity(
                            &services.sync,
                            &session.owner,
                            session.generation
                        ),
                    )
                })
                .await;
            let _ = page.update(cx, |page, cx| {
                page.loading = false;
                if let Ok((profile, identity)) = result {
                    if let Ok(profile) = profile { page.profile = Some(profile); }
                    if let Ok(identity) = identity { page.identity = Some(identity); }
                }
                if std::mem::take(&mut page.edit_requested) {
                    if page.identity.is_some() {
                        page.edit(cx);
                    } else {
                        page.notice = Some("desktop.personal.loadFailed");
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Открывает текущую версию значений в полях редактирования.
    fn edit(&mut self, cx: &mut Context<Self>) {
        let Some(identity) = &self.identity else {
            self.notice = Some("desktop.personal.loading");
            self.edit_requested = true;
            self.load(cx);
            cx.notify();
            return;
        };
        let values = [
            identity.first_name.as_deref(),
            identity.last_name.as_deref(),
            identity.phone.as_deref(),
        ];
        for (field, value) in self.fields.iter().zip(values) {
            field.update(cx, |field, cx| {
                field.set_text(value.unwrap_or_default(), cx)
            });
        }
        self.description.update(cx, |field, cx| {
            field.show_reply(identity.description.as_deref().unwrap_or_default(), cx)
        });
        self.editing = true;
        self.notice = None;
        cx.notify();
    }

    /// Возвращает карточку к просмотру и убирает неподтверждённое фото.
    fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.waiting.is_none() {
            self.editing = false;
            self.photo = None;
            self.notice = None;
            cx.notify();
        }
    }

    /// Принимает локальную правку аватара из отдельной модалки.
    fn select_photo(&mut self, photo: AvatarChoice, cx: &mut Context<Self>) {
        self.photo = Some(photo);
        self.notice = None;
        cx.notify();
    }

    /// Открывает компактную модалку фото и сохраняет фокус кнопки редактирования.
    fn open_avatar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.waiting.is_some() {
            return;
        }
        let host_link = self.host.clone();
        let personal_link = cx.weak_entity();
        let focus = self.photo_focus.clone();
        let existing = match &self.photo {
            Some(AvatarChoice::Remove) => false,
            Some(AvatarChoice::Upload(_, _)) => true,
            None => self
                .profile
                .as_ref()
                .and_then(UserProfile::avatar_file_id)
                .is_some(),
        };
        let _ = self.host.update(cx, |host, cx| {
            host.open_modal(&self.session, focus, window, cx, |_, cx| {
                cx.new(|_| AvatarDialog::new(host_link, personal_link, existing))
            });
        });
    }

    /// Отправляет подтверждённые правки. Сбой сохраняет заполненные поля.
    fn save(&mut self, cx: &mut Context<Self>) {
        if self.waiting.is_some() {
            return;
        }
        let Some(identity) = self.identity.as_ref() else {
            return;
        };
        let [first, last, phone] = self.fields.each_ref().map(|field| field.read(cx).text());
        let edit = PersonalEdit::new(
            identity,
            first,
            last,
            phone,
            self.description.read(cx).text(),
        );
        if !edit.is_valid() {
            self.notice = Some("desktop.personal.invalidIdentity");
            cx.notify();
            return;
        }
        let runtime = self.runtime.clone();
        let session = self.session.clone();
        let photo = self.photo.clone();
        self.notice = None;
        for field in &self.fields {
            field.update(cx, |field, cx| field.set_enabled(false, cx));
        }
        self.waiting = Some(cx.spawn(async move |page, cx| {
            let result = runtime
                .request(move |services| async move {
                    // Аватар сохраняется первым, как в оригинале. Результаты записываются отдельно.
                    let mut avatar = None;
                    if let Some(photo) = photo {
                        let upload = match photo {
                            AvatarChoice::Remove => None,
                            AvatarChoice::Upload(bytes, _) => Some(Arc::unwrap_or_clone(bytes)),
                        };
                        avatar = Some(
                            services
                                .messenger
                                .edit_own_avatar(
                                    &services.sync,
                                    &session.owner,
                                    session.generation,
                                    upload,
                                )
                                .await?,
                        );
                    }
                    let identity = if edit.is_empty() {
                        None
                    } else {
                        Some(
                            services
                                .messenger
                                .edit_own_identity(
                                    &services.sync,
                                    &session.owner,
                                    session.generation,
                                    edit,
                                )
                                .await,
                        )
                    };
                    Ok::<_, workspace_core::messenger::details::DetailsError>((avatar, identity))
                })
                .await;
            let _ = page.update(cx, |page, cx| {
                page.waiting = None;
                for field in &page.fields {
                    field.update(cx, |field, cx| field.set_enabled(true, cx));
                }
                if let Ok(Ok((avatar, identity))) = result {
                    if let Some(avatar) = avatar {
                        page.profile = Some(avatar);
                        page.photo = None;
                    }
                    if let Some(identity) = identity {
                        match identity {
                            Ok(identity) => page.identity = Some(identity),
                            Err(_) => {
                                page.notice = Some("desktop.personal.saveFailed");
                                cx.notify();
                                return;
                            }
                        }
                    }
                    page.cancel(cx);
                    let name = page.name(cx);
                    let _ = page.client.update(cx, |client, cx| {
                        client.set_profile_name(&page.session, name, page.profile.as_ref(), cx)
                    });
                } else {
                    page.notice = Some("desktop.personal.saveFailed");
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Возвращает сохранённое имя или введённый предпросмотр.
    fn name(&self, app: &App) -> String {
        if let Some(identity) = &self.identity {
            let values = if self.editing {
                [
                    self.fields[0].read(app).text(),
                    self.fields[1].read(app).text(),
                ]
            } else {
                [
                    identity.first_name.as_deref().unwrap_or_default(),
                    identity.last_name.as_deref().unwrap_or_default(),
                ]
            };
            let parts = values.map(str::trim);
            let name = parts
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            return if name.is_empty() {
                identity.username.clone()
            } else {
                name
            };
        }
        self.profile
            .as_ref()
            .map(|profile| profile.display_name.clone())
            .unwrap_or_else(|| self.caption.clone())
    }

    /// Рисует круглую фотографию и основные сведения над списком деталей.
    fn header(&self, palette: Palette, cx: &Context<Self>) -> Div {
        let language = Preferences::read(cx).language;
        let name = self.name(cx);
        let initials = name
            .graphemes(true)
            .next()
            .map(str::to_uppercase)
            .unwrap_or_default();
        let mut avatar = Avatar::from_initials(initials);
        if !self.editing {
            avatar.presence = match self.profile.as_ref().and_then(|profile| profile.presence) {
                Some(ProfilePresence::Active) => AvatarPresence::Online,
                Some(ProfilePresence::Idle | ProfilePresence::DoNotDisturb) => AvatarPresence::Away,
                _ => AvatarPresence::Hidden,
            };
        }
        avatar.image = match &self.photo {
            Some(AvatarChoice::Remove) => None,
            Some(AvatarChoice::Upload(_, image)) => Some(image.clone().into()),
            None => self
                .session
                .user_uuid()
                .and_then(|id| self.avatar.image(id, cx)),
        };
        let has_image = avatar.image.is_some();
        let avatar = avatar
            .render_at(palette, px(64.), palette.surface)
            .when(has_image, |photo| {
                photo.debug_selector(|| "profile-personal-avatar-photo".into())
            });
        let avatar = if self.editing && self.waiting.is_none() {
            activate(
                div()
                    .id("profile-edit-avatar")
                    .debug_selector(|| "profile-edit-avatar".into())
                    .relative()
                    .flex_shrink_0()
                    .child(avatar)
                    .child(
                        div()
                            .absolute()
                            .bottom_0()
                            .right_0()
                            .size(px(20.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(palette.icon)
                            .child(Icon::Stylus.element(px(14.), palette.surface)),
                    ),
                Some(&self.photo_focus),
                palette,
                self.handler(cx, Self::open_avatar),
            )
            .into_any_element()
        } else {
            avatar.into_any_element()
        };
        let profile = self.profile.as_ref();
        div()
            .flex()
            .items_center()
            .gap_4()
            .px_2()
            .pt_2()
            .child(avatar)
            .child(if self.editing {
                div()
                    .flex_1()
                    .min_w_0()
                    .child(label(
                        palette,
                        TextRole::Caption,
                        language.tr("settings.profileDisplayName"),
                    ))
                    .child(
                        label(palette, TextRole::Caption, name)
                            .h(px(32.))
                            .px_2()
                            .flex()
                            .items_center()
                            .rounded(px(6.))
                            .bg(palette.card)
                            .text_color(palette.text)
                            .truncate(),
                    )
            } else {
                div()
                    .flex_1()
                    .min_w_0()
                    .h(px(64.))
                    .flex()
                    .flex_col()
                    .justify_between()
                    .child(label(palette, TextRole::ProfileHeading, name).truncate())
                    .children(
                        profile
                            .and_then(UserProfile::status_label)
                            .map(|status| label(palette, TextRole::Caption, status).truncate()),
                    )
                    .children(
                        profile
                            .and_then(|profile| profile.presence)
                            .map(|presence| {
                                let (text, color) = match presence {
                                    ProfilePresence::Active => {
                                        (language.tr("a11y.online"), palette.presence_online)
                                    }
                                    ProfilePresence::Idle => {
                                        (language.tr("a11y.away"), palette.presence_away)
                                    }
                                    ProfilePresence::DoNotDisturb => {
                                        (language.tr("presence.doNotDisturb"), palette.danger)
                                    }
                                    ProfilePresence::Offline => {
                                        (language.tr("presence.offline"), palette.text_muted)
                                    }
                                };
                                label(palette, TextRole::Caption, text)
                                    .line_height(px(16.))
                                    .text_color(color)
                            }),
                    )
            })
    }

    /// Передаёт действие кнопки текущей карточке.
    fn handler(
        &self,
        cx: &Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl Fn(&mut Window, &mut App) + 'static {
        let weak = cx.weak_entity();
        move |window, app| {
            let _ = weak.update(app, |view, cx| action(view, window, cx));
        }
    }

    /// Показывает избранное, редактор и копирование ссылки на свой профиль.
    fn actions(&self, palette: Palette, cx: &Context<Self>) -> Div {
        let language = Preferences::read(cx).language;
        let link = self
            .session
            .user_uuid()
            .and_then(|id| sidebar_profile_link(&self.session.owner, id).ok());
        let client = self.client.clone();
        let session = self.session.clone();
        div()
            .flex()
            .gap_2()
            .child(action_tile(
                "profile-favorites",
                language.tr("common.favorites"),
                (Icon::Home, 26.),
                palette,
                move |window, app| {
                    let _ = client.update(app, |client, cx| {
                        client.open_favorites(session.clone(), window, cx);
                    });
                },
            ))
            .child(action_tile(
                "profile-edit",
                language.tr("info.edit"),
                (Icon::Stylus, 30.),
                palette,
                self.handler(cx, |view, _, cx| view.edit(cx)),
            ))
            .child(action_tile(
                "profile-share",
                if self.share_copied {
                    language.tr("message.copied")
                } else {
                    language.tr("info.share")
                },
                if self.share_copied {
                    (Icon::Check, 28.)
                } else {
                    (Icon::Links, 40.)
                },
                palette,
                self.handler(cx, move |view, _, cx| {
                    if let Some(link) = link.clone() {
                        cx.write_to_clipboard(ClipboardItem::new_string(link));
                        view.share_copied = true;
                        let delay = cx.background_executor().timer(Duration::from_secs(2));
                        view.share_feedback = Some(cx.spawn(async move |view, cx| {
                            delay.await;
                            let _ = view.update(cx, |view, cx| {
                                view.share_copied = false;
                                cx.notify();
                            });
                        }));
                        cx.notify();
                    }
                }),
            ))
    }

    /// Показывает подробности в том же порядке и выделяет доступные для копирования строки.
    fn details(&self, palette: Palette, language: Language, app: &App) -> Div {
        let detail = |title, icon, value, copyable, palette| {
            detail(title, icon, value, copyable, palette, language)
        };
        let identity = self.identity.as_ref();
        let profile = self.profile.as_ref();
        let email = identity
            .and_then(|user| user.email.as_deref())
            .or_else(|| profile.and_then(|user| user.email.as_deref()));
        let phone = identity.map(|user| {
            user.phone
                .as_deref()
                .filter(|value| !value.is_empty())
                .unwrap_or("-")
        });
        let created = profile
            .and_then(|user| user.created_at.as_deref())
            .and_then(|date| profile_joined_date(date, language));
        let mut details = div().flex().flex_col().gap_3();
        if self.editing {
            details = details
                .child(edit_field(
                    language.tr("settings.profileFirstName"),
                    Icon::Account,
                    &self.fields[0],
                    palette,
                ))
                .child(edit_field(
                    language.tr("settings.profileLastName"),
                    Icon::Account,
                    &self.fields[1],
                    palette,
                ))
                .child(detail(
                    language.tr("info.username"),
                    Icon::At,
                    identity.map(|user| user.username.as_str()),
                    true,
                    palette,
                ));
        }
        details = details
            .child(detail(
                language.tr("info.userId"),
                Icon::At,
                Some(&self.session.owner.user_id),
                true,
                palette,
            ))
            .child(detail(
                language.tr("common.email"),
                Icon::Mail,
                email,
                true,
                palette,
            ));
        details = if self.editing {
            details.child(edit_field(
                language.tr("info.phone"),
                Icon::Phone,
                &self.fields[2],
                palette,
            ))
        } else {
            details.child(detail(
                language.tr("info.phone"),
                Icon::Phone,
                phone,
                false,
                palette,
            ))
        };
        for (title, icon, value) in [
            (language.tr("info.jobTitle"), Icon::BusinessCenter, None),
            (language.tr("info.manager"), Icon::Handshake, None),
            (language.tr("info.role"), Icon::Account, None),
            (language.tr("info.accountType"), Icon::Group, None),
            (language.tr("info.accountStatus"), Icon::Info, None),
            (language.tr("info.timezone"), Icon::GlobeLocation, None),
            (language.tr("info.localTime"), Icon::Calendar, None),
            (
                language.tr("info.joined"),
                Icon::Calendar,
                created.as_deref(),
            ),
            (language.tr("info.birthday"), Icon::Celebration, None),
        ] {
            details = details.child(detail(title, icon, value, false, palette));
        }
        if self.editing {
            details.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(label(
                        palette,
                        TextRole::Caption,
                        language.tr("settings.profileDescription"),
                    ))
                    .child(
                        div()
                            .min_h(px(64.))
                            .px_2()
                            .py_2()
                            .rounded(px(6.))
                            .border_1()
                            .border_color(palette.border)
                            .bg(palette.card)
                            .child(if self.waiting.is_some() {
                                label(
                                    palette,
                                    TextRole::Body,
                                    self.description.read(app).text().to_owned(),
                                )
                                .into_any_element()
                            } else {
                                self.description.clone().into_any_element()
                            }),
                    ),
            )
        } else {
            details.children(
                identity
                    .and_then(|user| user.description.as_deref())
                    .filter(|value| !value.is_empty())
                    .map(|value| {
                        detail(
                            language.tr("settings.profileDescription"),
                            Icon::Info,
                            Some(value),
                            false,
                            palette,
                        )
                    }),
            )
        }
    }

    /// Размещает отмену и сохранение внизу формы.
    fn footer(&self, palette: Palette, cx: &Context<Self>) -> Div {
        let language = Preferences::read(cx).language;
        let mut cancel = Button::new("profile-edit-cancel", language.tr("common.cancel"));
        let mut save = Button::new("profile-edit-save", language.tr("info.saveChanges"));
        save.appearance = ButtonAppearance::Primary;
        save.disabled = self.waiting.is_some();
        cancel.disabled = save.disabled;
        div()
            .flex()
            .gap_2()
            .pb_2()
            .child(
                cancel
                    .render(
                        palette,
                        None,
                        self.handler(cx, |view, _, cx| view.cancel(cx)),
                    )
                    .w(px(110.))
                    .h(px(40.)),
            )
            .child(
                save.render(palette, None, self.handler(cx, |view, _, cx| view.save(cx)))
                    .flex_1()
                    .min_w_0()
                    .h(px(40.)),
            )
    }
}

impl Render for Personal {
    /// В режиме просмотра показывает карточку, в режиме правки — те же данные и поля.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_window(window, cx);
        let language = Preferences::read(cx).language;
        div()
            .id("profile-personal-info-page")
            .debug_selector(|| "profile-personal-info-page".into())
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_2()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_5()
                    .child(self.header(palette, cx))
                    .when(!self.editing, |body| body.child(self.actions(palette, cx)))
                    .children(self.notice.map(|notice| {
                        label(palette, TextRole::Body, language.tr(notice))
                            .px_2()
                            .text_color(palette.danger)
                    }))
                    .when(self.editing, |body| {
                        body.child(div().h(px(1.)).bg(palette.border))
                    })
                    .child(self.details(palette, language, cx))
                    .when(self.editing, |body| body.child(self.footer(palette, cx))),
            )
    }
}

/// Рисует плитку действия под аватаром.
fn action_tile(
    id: &'static str,
    text: &str,
    (icon, size): (Icon, f32),
    palette: Palette,
    action: impl Fn(&mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    activate(
        div()
            .id(id)
            .debug_selector(move || id.into())
            .h(px(72.))
            .flex_1()
            .min_w_0()
            .when(id == "profile-edit", |tile| tile.flex_none().w(px(104.4)))
            .px_2()
            .rounded(px(6.))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .bg(palette.card)
            .hover(|style| style.bg(palette.background))
            .child(
                div()
                    .size(px(40.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(icon.element(px(size), palette.text)),
            )
            .child(
                label(palette, TextRole::Caption, text.to_owned())
                    .line_height(px(16.))
                    .whitespace_nowrap()
                    .truncate(),
            ),
        None,
        palette,
        action,
    )
}

/// Соединяет иконку, подпись, значение и кнопку копирования.
fn detail(
    title: &str,
    icon: Icon,
    value: Option<&str>,
    copyable: bool,
    palette: Palette,
    language: Language,
) -> Div {
    let action_value = value.filter(|_| copyable).map(str::to_owned);
    div()
        .flex()
        .items_center()
        .gap_4()
        .min_h(px(36.))
        .px_1()
        .child(icon.element(px(24.), palette.icon))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(label(palette, TextRole::Caption, title.to_owned()).line_height(px(16.)))
                .child(match value {
                    Some(value) => ellipsized_label(palette, TextRole::Body, value.to_owned()),
                    None => label(
                        palette,
                        TextRole::Body,
                        language.tr("desktop.personal.favoriteUnavailable"),
                    )
                    .italic()
                    .text_color(palette.text_muted)
                    .truncate(),
                }),
        )
        .children(action_value.map(|value| {
            let mut button = Button::new(
                gpui::SharedString::from(format!("profile-copy-{title}")),
                "",
            );
            button.appearance = ButtonAppearance::Ghost;
            button.icon = Some(Icon::Copy);
            button.render(palette, None, move |_, app| {
                app.write_to_clipboard(ClipboardItem::new_string(value.clone()))
            })
        }))
}

/// Показывает то же место в списке как поле ввода.
fn edit_field(title: &str, icon: Icon, field: &Entity<TextInput>, palette: Palette) -> Div {
    div()
        .flex()
        .items_end()
        .gap_4()
        .px_1()
        .child(icon.element(px(24.), palette.icon).mb_1())
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(label(palette, TextRole::Caption, title.to_owned()).mb_1())
                .child(field.clone()),
        )
}
