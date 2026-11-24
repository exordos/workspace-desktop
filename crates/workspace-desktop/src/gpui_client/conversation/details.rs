//! Управляет открытием, фокусом, загрузкой и действиями правой панели разговора.

mod contact;
mod encrypted_settings;
mod encryption;
mod gallery;
mod picker;
mod presentation;
mod source;
mod summary_settings;
mod topic_actions;
mod view;

use std::{collections::HashMap, sync::Arc, time::Duration};

use gpui::{
    App, ClipboardItem, Context, Div, Entity, EntityId, FocusHandle, IntoElement, Render, Stateful,
    Task, WeakEntity, Window, div, prelude::*,
};
use uuid::Uuid;
use workspace_core::{
    messenger::{
        catalog::Chat,
        details::{SidebarDetails, TopicSummaryBlock, parse_topic_summary, sidebar_profile_link},
    },
    storage::media::GalleryCounts,
};

use super::{
    Conversation, DetailsProfileAction, DetailsProfileIntent, Failure, OpenDetailsTopic,
    heading::icon_button,
};
use crate::gpui_client::{
    avatars::AvatarImages,
    contracts::{AccountSession, ConversationAddress},
    preferences::{Language, Preferences},
    right_panel::{ConversationBackButton, PanelKey, RightPanel, Scope},
    runtime::RuntimeClient,
    ui::{Button, Icon, Palette, TextRole, activate, label},
};
pub(in crate::gpui_client) use contact::{ContactOrigin, ContactPanel, ContactRequest, ContactTarget};
use gallery::Gallery;
use picker::{Picker, PickerDialog};
use source::{Mutation, Problem};
use topic_actions::TopicMenu;
use view::PanelAction;
pub(in crate::gpui_client) use view::member_counts;

/// Сведения видны только между открытием панели и закрытием.
enum State {
    Loading {
        op_id: Uuid,        // Номер запроса, который принимает это открытие панели.
        _request: Task<()>, // Закрытие отменяет ожидание ответа в окне.
    },
    Ready,           // Есть подтверждённый снимок, который можно показывать.
    Failed(Problem), // Сбой и допустимый следующий шаг.
}

/// Действие, рядом с которым экран показывает ошибку сервера.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MutationKind {
    Notifications, // Изменение режима уведомлений.
    Add,           // Добавление выбранных участников.
    Remove,        // Удаление привязки.
    Topic,         // Изменение темы из контекстного меню.
    KeySharing,    // Политика выдачи ключей участниками защищённого чата.
}

/// Вложенный экран сведений о защищённом чате.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EncryptedScreen {
    Settings,       // Сведения о канале и политика выдачи ключей.
    Keys,           // Текущий ключ, ключи устройства и запросы.
    LocalKey(Uuid), // Сведения о выбранном ключе устройства.
}

/// Изменение, которое выполняется для открытой панели.
struct PendingMutation {
    id: Uuid, // Связывает ответ с запуском из текущего открытия.
}

/// Хранит состояние и фокус панели одного экземпляра экрана.
pub(super) struct Panel {
    host: Entity<RightPanel>,              // Общий стек правых панелей окна.
    key: PanelKey,                         // Ключ текущего открытия разговора.
    body: Option<EntityId>,                // Идентификатор живого содержимого в стеке.
    _host_changes: gpui::Subscription,     // Сбрасывает данные после закрытия через общую рамку.
    runtime: RuntimeClient,                // Тот же рабочий поток, что читает историю.
    state: Option<State>,                  // None, пока панель закрыта.
    avatars: AvatarImages,                 // Подписки панели на общие фото по UUID человека.
    snapshot: Option<Arc<SidebarDetails>>, // Последний снимок до закрытия или сброса.
    keys: encryption::KeyManagement, // Локальные ключи только подтверждённого защищённого чата.
    encrypted_screens: Vec<(EncryptedScreen, FocusHandle)>, // Вложенные экраны и открывшие их кнопки.
    encrypted_settings_entry: FocusHandle,                  // Кнопка настроек в сведениях.
    encrypted_settings_keys: FocusHandle,                   // Переход к ключам из настроек.
    encrypted_settings_sharing: FocusHandle, // Переключатель выдачи ключей в настройках.
    catalog: Option<Arc<[Chat]>>,    // Превью для порядка тем из каталога того же входа.
    reopen_on_access: bool,          // Перенос открытой панели при переходе в другую тему канала.
    topics_expanded: bool,           // Показ списка тем в текущей панели.
    summary_expanded: Option<bool>,  // Выбор пользователя, пока открыт один контекст.
    summary_content: Option<Arc<[TopicSummaryBlock]>>, // Разобранный Markdown выбранной темы.
    summary_settings: FocusHandle,   // Кнопка открытия настроек контекста темы.
    topic_menu: TopicMenu,           // Контекстное меню и клавиатурный фокус строк.
    share_feedback: Option<Task<()>>, // Галочка после копирования ссылки.
    copy_feedback: HashMap<usize, Task<()>>, // Галочки скопированных строк до окончания таймера.
    mutation: Option<PendingMutation>, // Текущее изменение, пока ждём подтверждение сервера.
    mutation_error: Option<MutationKind>, // Последняя ошибка рядом с действием.
    picker: Option<Picker>,          // Выбор участников внутри панели владельца.
    add_members_focus: FocusHandle,  // Кнопка открытия окна выбора участников.
    gallery: Option<Gallery>,                        // Выбранный раздел вложений и его страницы.
    gallery_triggers: [FocusHandle; 4],              // Кнопки категорий для возврата после галереи.
    gallery_counts: Option<GalleryCounts>,           // Числа известной истории.
    gallery_counts_request: Option<(Uuid, Task<()>)>, // Текущий запрос чисел для этой панели.
    refocus_details: Option<FocusHandle>, // Кнопка, открывшая предыдущий вложенный экран.
    title_trigger: FocusHandle,      // Заголовок может открыть ту же панель.
    trigger: FocusHandle,            // Кнопка сведений в шапке разговора.
    retry: FocusHandle,              // Кнопка повтора после временного сбоя.
}

impl Panel {
    /// Возвращает общий оверлей для диалогов текущего разговора.
    pub(super) fn modal_host(&self) -> Entity<RightPanel> {
        self.host.clone()
    }

    /// Даёт диалогу доступные чаты из каталога того же входа.
    pub(super) fn catalog_chats(&self) -> &[Chat] {
        self.catalog.as_deref().unwrap_or_default()
    }

    /// Даёт истории имена из уже доступных снимков для старых упоминаний.
    pub(super) fn mention_names(&self) -> super::inline_references::MentionNames {
        super::inline_references::MentionNames {
            chats: self.catalog.clone(),
            details: self.snapshot.clone(),
        }
    }

    /// Готовит закрытую панель без сетевого запроса.
    pub(super) fn new(
        host: Entity<RightPanel>,
        address: ConversationAddress,
        runtime: RuntimeClient,
        cx: &mut Context<Conversation>,
    ) -> Self {
        cx.on_release(|view, app| {
            if let Some(panel) = view.details.as_mut() {
                panel.release_gallery(app);
            }
        })
        .detach();
        let host_changes = cx.observe(&host, |view, host, cx| {
            let mut closed_gallery = false;
            if let Some(panel) = view.details.as_mut()
                && panel
                    .body
                    .is_some_and(|id| !host.read(cx).contains_view(id))
            {
                closed_gallery = panel.gallery.is_some();
                panel.reset(cx);
            }
            if let Some(panel) = view.details.as_mut()
                && !host.read(cx).modal_is::<PickerDialog>()
            {
                panel.picker = None;
            }
            if closed_gallery {
                view.cancel_gallery_export();
            }
            cx.notify();
        });
        Self {
            host,
            key: PanelKey {
                scope: Scope::Conversation(address),
                id: "conversation-details".into(),
            },
            body: None,
            _host_changes: host_changes,
            runtime,
            state: None,
            avatars: AvatarImages::default(),
            snapshot: None,
            keys: encryption::KeyManagement::default(),
            encrypted_screens: Vec::new(),
            encrypted_settings_entry: cx.focus_handle(),
            encrypted_settings_keys: cx.focus_handle(),
            encrypted_settings_sharing: cx.focus_handle(),
            catalog: None,
            reopen_on_access: false,
            topics_expanded: true,
            summary_expanded: None,
            summary_content: None,
            summary_settings: cx.focus_handle(),
            topic_menu: TopicMenu::new(cx),
            share_feedback: None,
            copy_feedback: HashMap::new(),
            mutation: None,
            mutation_error: None,
            picker: None,
            add_members_focus: cx.focus_handle(),
            gallery: None,
            gallery_triggers: std::array::from_fn(|_| cx.focus_handle()),
            gallery_counts: None,
            gallery_counts_request: None,
            refocus_details: None,
            title_trigger: cx.focus_handle(),
            trigger: cx.focus_handle(),
            retry: cx.focus_handle(),
        }
    }

    /// Подключает снимок списка чатов с временем сообщений. Возвращает true при замене снимка.
    pub(super) fn set_catalog(&mut self, catalog: Option<&Arc<[Chat]>>) -> bool {
        match (&self.catalog, catalog) {
            (Some(current), Some(next)) if Arc::ptr_eq(current, next) => false,
            (None, None) => false,
            _ => {
                self.catalog = catalog.cloned();
                true
            }
        }
    }

    /// Убирает контекстные экраны из стека при сбросе или отзыве доступа.
    pub(super) fn clear(
        &mut self,
        address: &ConversationAddress,
        fallback: Option<&FocusHandle>,
        cx: &mut Context<Conversation>,
    ) {
        self.reset(cx);
        self.host.update(cx, |host, cx| {
            host.invalidate_conversation(address, fallback.unwrap_or(&self.trigger).clone(), cx)
        });
    }

    /// Проверяет, жив ли экран, в том числе пока он прикрыт другим экраном.
    fn is_present(&self, app: &App) -> bool {
        self.body
            .is_some_and(|id| self.host.read(app).contains_view(id))
    }

    /// Закрывает панель и отменяет ожидание её запросов в окне.
    fn reset(&mut self, app: &mut App) {
        self.body = None;
        if let Some(State::Loading { op_id, .. }) = self.state.as_ref() {
            log::debug!("gpui_details op_id={op_id} event=cancelled");
        }
        self.state = None;
        self.snapshot = None;
        self.keys = encryption::KeyManagement::default();
        self.encrypted_screens.clear();
        self.avatars.clear();
        self.reopen_on_access = false;
        self.topics_expanded = true;
        self.summary_expanded = None;
        self.summary_content = None;
        self.topic_menu.reset();
        self.share_feedback = None;
        self.copy_feedback.clear();
        self.mutation = None;
        self.mutation_error = None;
        self.picker = None;
        self.release_gallery(app);
        self.gallery_counts = None;
        self.gallery_counts_request = None;
        self.refocus_details = None;
    }

    /// Снимает вложенный экран и освобождает его миниатюры.
    fn release_gallery(&mut self, app: &mut App) {
        if let Some(mut gallery) = self.gallery.take() {
            gallery.release_previews(app);
        }
    }

    /// Возвращает экран, открытый поверх основных сведений о защищённом чате.
    fn encrypted_screen(&self) -> Option<EncryptedScreen> {
        self.encrypted_screens.last().map(|(screen, _)| *screen)
    }

    /// Сбрасывает настройки и ключи при смене режима стрима или потере снимка.
    fn clear_encrypted_screens(&mut self) {
        self.encrypted_screens.clear();
        self.keys = encryption::KeyManagement::default();
    }

    /// Разбирает контекст, только когда текст выбранной темы действительно изменился.
    fn sync_summary(&mut self, snapshot: &SidebarDetails, topic_id: Option<Uuid>) {
        let Some(summary) = selected_summary(snapshot, topic_id) else {
            self.summary_content = None;
            return;
        };
        if self.summary_content.is_none()
            || self.snapshot.as_deref().and_then(|current| selected_summary(current, topic_id))
                != Some(summary)
        {
            self.summary_content = Some(parse_topic_summary(summary).into());
        }
    }

    /// Оставляет подписки собеседника или участников текущего канала.
    fn refresh_avatars(
        &mut self,
        details: &SidebarDetails,
        address: &ConversationAddress,
        cx: &mut Context<Conversation>,
    ) {
        let users = match details {
            SidebarDetails::Peer(peer) => vec![peer.user_id],
            SidebarDetails::Channel(channel) => channel
                .members
                .iter()
                .map(|member| member.user_id)
                .collect(),
        };
        let session = AccountSession {
            owner: address.route.owner.clone(),
            generation: address.generation,
        };
        self.avatars.refresh(users, &self.runtime, &session, cx);
    }

    /// Рисует переключатель правой панели и подсказывает его текущее действие.
    pub(super) fn trigger(
        &self,
        locale: Language,
        direct: bool,
        palette: Palette,
        cx: &Context<Conversation>,
    ) -> Stateful<Div> {
        let hint = if self.host.read(cx).is_active(&self.key) {
            locale.tr("a11y.hidePanel")
        } else if direct {
            locale.tr("info.partnerInfo")
        } else {
            locale.tr("chatInfo.channelInfo")
        };
        icon_button(
            "conversation-details-trigger",
            Icon::SidePanel,
            hint.into(),
            &self.trigger,
            false,
            palette,
            action(cx, Conversation::toggle_details),
        )
    }

    /// Позволяет открыть сведения из блока названия без переключения уже открытой панели.
    pub(super) fn title_trigger(
        &self,
        title: Div,
        palette: Palette,
        cx: &Context<Conversation>,
    ) -> Div {
        div()
            .relative()
            .min_w_0()
            .child(title)
            .child(activate(
                div()
                    .id("conversation-details-title")
                    .debug_selector(|| "conversation-details-title".into())
                    .absolute()
                    .inset_0()
                    .rounded(gpui::px(8.))
                    .border_1()
                    .border_color(gpui::transparent_black()),
                Some(&self.title_trigger),
                palette,
                action(cx, Conversation::open_details_from_title),
            ))
    }

    /// Рисует содержимое внутри общей рамки стека.
    fn render_content(
        &self,
        palette: Palette,
        view: &Conversation,
        cx: &Context<Conversation>,
    ) -> Div {
        let Some(state) = self.state.as_ref() else {
            return div();
        };
        let language = Preferences::read_for(cx, &view.address.route.owner).language;
        let target = cx.entity().downgrade();
        let dispatch = move |intent, window: &mut Window, app: &mut App| {
            let _ = target.update(app, |screen, cx| {
                screen.handle_panel_action(intent, window, cx)
            });
        };
        let gallery_triggers = view
            .gallery_access()
            .is_some()
            .then_some(view::GalleryTriggers {
                focus: &self.gallery_triggers,
                counts: self.gallery_counts,
            });
        let content = if let Some(gallery) = &self.gallery {
            view::render_gallery(view, gallery, palette, language, dispatch)
        } else if let Some(SidebarDetails::Channel(channel)) = self.snapshot.as_deref()
            && channel.encrypted
        {
            match self.encrypted_screen() {
                Some(EncryptedScreen::Settings) => view::encrypted_scroll(
                    "encrypted-settings-scroll",
                    encrypted_settings::render(self, channel, palette, language, cx),
                ),
                Some(EncryptedScreen::Keys) => view::encrypted_scroll(
                    "encrypted-keys-scroll",
                    self.keys.render_keys(self, palette, language, cx),
                ),
                Some(EncryptedScreen::LocalKey(id)) => view::encrypted_scroll(
                    "encrypted-local-key-scroll",
                    self.keys.render_local_key(id, self, palette, language, cx),
                ),
                None => view::render_encrypted(
                    &presentation::channel_content(self, channel, &view.address, cx),
                    self.encrypted_actions(view, palette, language, cx),
                    self.keys.render_info(self, palette, language, cx),
                    palette,
                    gallery_triggers,
                    dispatch,
                )
                .debug_selector(|| "conversation-details-ready".into()),
            }
        } else if let Some(details) = self.snapshot.as_deref() {
            let data = presentation::content(
                self,
                details,
                &view.address,
                cx,
                view.peer_call_availability(cx),
            );
            view::render(&data, palette, gallery_triggers, dispatch)
                .debug_selector(|| "conversation-details-ready".into())
        } else {
            match state {
                State::Loading { .. } | State::Ready => label(
                    palette,
                    TextRole::Body,
                    language.tr("desktop.conversation.details.loading"),
                )
                .debug_selector(|| "conversation-details-loading".into()),
                State::Failed(problem) => self
                    .render_problem(*problem, palette, language, cx)
                    .debug_selector(|| "conversation-details-error".into()),
            }
        };
        div()
            .min_h_0()
            .min_w_0()
            .flex()
            .flex_col()
            .flex_1()
            .overflow_hidden()
            .px_2()
            .child(content)
    }

    /// Показывает равные по ширине кнопки звонка и настроек защищённого чата.
    fn encrypted_actions(
        &self,
        view: &Conversation,
        palette: Palette,
        language: Language,
        cx: &Context<Conversation>,
    ) -> Div {
        let focus = self.encrypted_settings_entry.clone();
        let opener = focus.clone();
        let hint = language.tr("settings.settings");
        let settings = Button::new("conversation-encrypted-settings", "")
            .render(
                Palette {
                    card: palette.accent_soft,
                    ..palette
                },
                Some(&focus),
                action(cx, move |view, window, cx| {
                    view.open_encrypted_screen(
                        EncryptedScreen::Settings,
                        opener.clone(),
                        window,
                        cx,
                    );
                }),
            )
            .h(gpui::px(48.))
            .flex_1()
            .child(Icon::InstantMix.element(gpui::px(32.), palette.text))
            .tooltip(move |_, app| {
                crate::gpui_client::ui::popup::tooltip(palette, hint.into(), app)
            })
            .debug_selector(|| "conversation-encrypted-settings".into());
        div()
            .flex()
            .gap_2()
            .px_2()
            .child(view.protected_call_button(palette, cx))
            .child(settings)
    }

    /// Перерисовывает заголовок общей рамки после смены содержимого.
    pub(super) fn refresh_frame(&self, cx: &mut Context<Conversation>) {
        self.host.update(cx, |_, cx| cx.notify());
    }

    /// Выбирает локализованный заголовок для подтверждённого типа разговора.
    fn title(&self, address: &ConversationAddress, app: &App) -> &'static str {
        let locale = Preferences::read_for(app, &address.route.owner).language;
        if let Some(gallery) = &self.gallery {
            return view::gallery_title(gallery.category, locale);
        }
        if let Some(screen) = self.encrypted_screen() {
            return locale.tr(match screen {
                EncryptedScreen::Settings => "channel.editChannel",
                EncryptedScreen::Keys => "e2ee.keyTitle",
                EncryptedScreen::LocalKey(_) => "e2ee.localKeyTitle",
            });
        }
        match self.snapshot.as_deref() {
            Some(SidebarDetails::Peer(peer)) if peer.is_self => locale.tr("settings.personalInfo"),
            Some(SidebarDetails::Channel(channel)) if channel.direct_peer.is_some() => {
                locale.tr("info.information")
            }
            Some(SidebarDetails::Channel(_)) => locale.tr("chatInfo.channelInfo"),
            None if address.route.scope.topic_id.is_some()
                && !self
                    .catalog_chats()
                    .iter()
                    .any(|chat| chat.id == address.route.scope.stream_id && chat.direct) =>
            {
                locale.tr("chatInfo.channelInfo")
            }
            _ => locale.tr("info.information"),
        }
    }

    /// Показывает повтор только для временной ошибки.
    fn render_problem(
        &self,
        problem: Problem,
        palette: Palette,
        language: Language,
        cx: &Context<Conversation>,
    ) -> Div {
        let message = language.tr(match problem {
            Problem::AccessLost => "desktop.conversation.details.conversationUnavailable",
            Problem::Unsupported => "desktop.conversation.details.unsupported",
            Problem::ProfileUnavailable => "desktop.conversation.details.profileUnavailable",
            Problem::Unavailable => "chatInfo.loadError",
        });
        div()
            .flex()
            .flex_col()
            .p_2()
            .items_start()
            .gap_3()
            .child(label(palette, TextRole::Body, message))
            .when(matches!(problem, Problem::Unavailable), |content| {
                content.child(
                    Button::new("conversation-details-retry", language.tr("common.retry"))
                        .render(
                            palette,
                            Some(&self.retry),
                            action(cx, |view, window, cx| {
                                if let Some(panel) = &view.details {
                                    panel.host.read(cx).focus_close(&panel.key, window);
                                }
                                view.request_details(cx);
                            }),
                        )
                        .debug_selector(|| "conversation-details-retry".into()),
                )
            })
    }
}

/// Берёт контекст только выбранной темы из снимка текущего канала.
fn selected_summary(snapshot: &SidebarDetails, topic_id: Option<Uuid>) -> Option<&str> {
    let SidebarDetails::Channel(channel) = snapshot else {
        return None;
    };
    if channel.encrypted {
        return None;
    }
    let topic_id = topic_id?;
    channel.topics.iter().find(|topic| topic.id == topic_id)?.summary.as_deref()
}

/// Содержимое общей рамки, связанное с живым разговором.
struct DetailsBody {
    conversation: WeakEntity<Conversation>, // Закрывается вместе с исходным маршрутом.
    _changes: gpui::Subscription,           // Перерисовывает содержимое после обновления данных.
}

impl Render for DetailsBody {
    /// Перерисовывает содержимое и возвращает фокус после закрытия вложенного экрана.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_window(window, cx);
        self.conversation
            .update(cx, |view, cx| {
                let Some(panel) = view.details.as_mut() else {
                    return div();
                };
                if let Some(focus) = panel.refocus_details.take() {
                    window.focus(&focus);
                }
                let mut content = view.details
                    .as_ref()
                    .map(|panel| panel.render_content(palette, view, cx))
                    .unwrap_or_else(div);
                if let Some(layer) = view.render_topic_menu(window, palette, cx) {
                    content = content.children(layer);
                }
                content
            })
            .unwrap_or_else(|_| div())
    }
}

impl Conversation {
    /// Открывает следующий экран из текущих сведений защищённого чата.
    fn open_encrypted_screen(
        &mut self,
        screen: EncryptedScreen,
        focus: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.details_active(cx) {
            return;
        }
        let Some(panel) = &mut self.details else {
            return;
        };
        if panel.gallery.is_some()
            || !matches!(panel.snapshot.as_deref(), Some(SidebarDetails::Channel(channel)) if channel.encrypted)
            || !matches!(
                (panel.encrypted_screen(), screen),
                (None, EncryptedScreen::Settings)
                    | (Some(EncryptedScreen::Settings), EncryptedScreen::Keys)
                    | (Some(EncryptedScreen::Keys), EncryptedScreen::LocalKey(_))
            )
        {
            return;
        }
        panel.encrypted_screens.push((screen, focus));
        panel.refocus_details = None;
        if let Some(back) = panel.host.read(cx).back_focus(&panel.key) {
            window.focus(&back);
        }
        panel.refresh_frame(cx);
        cx.notify();
    }

    /// Возвращает к списку после удаления открытого ключа с устройства.
    fn return_from_encrypted_local_key(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if let Some(panel) = &mut self.details
            && panel.encrypted_screen() == Some(EncryptedScreen::LocalKey(id))
        {
            panel.encrypted_screens.pop();
            panel.refocus_details = panel.host.read(cx).back_focus(&panel.key);
            panel.refresh_frame(cx);
            cx.notify();
        }
    }

    /// Открывает сведения через общую рамку или закрывает экран, если он сверху.
    pub(in crate::gpui_client) fn toggle_details(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_open_details() {
            return;
        }
        let Some(panel) = &mut self.details else {
            return;
        };
        if panel.host.read(cx).is_active(&panel.key) {
            let closed_gallery = panel.gallery.is_some();
            panel.reset(cx);
            panel.host.update(cx, |host, cx| host.pop(window, cx));
            if closed_gallery {
                self.cancel_gallery_export();
            }
        } else {
            self.load_details(false, window, cx);
        }
    }

    /// Показывает кнопку только в разговоре, к которому подтверждён доступ.
    pub(super) fn can_open_details(&self) -> bool {
        self.has_access() && !self.favorites_presentation
    }

    /// Проверяет, что сведения видимы и могут отправить действие в рабочее окно.
    pub(in crate::gpui_client) fn details_active(&self, app: &App) -> bool {
        self.can_open_details()
            && self
                .details
                .as_ref()
                .is_some_and(|panel| panel.host.read(app).is_active(&panel.key))
    }

    /// Открывает правую панель заголовком и запоминает эту точку фокуса.
    fn open_details_from_title(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.can_open_details() {
            self.load_details(true, window, cx);
        }
    }

    /// Добавляет содержимое в стек и начинает загрузку только для нового открытия.
    fn load_details(
        &mut self,
        from_title: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = &mut self.details else {
            return;
        };
        let conversation = cx.entity();
        let owner = self.address.route.owner.clone();
        let mut opened = None;
        panel.host.update(cx, |host, cx| {
            host.open(
                panel.key.clone(),
                panel.title(&self.address, cx),
                if from_title { panel.title_trigger.clone() } else { panel.trigger.clone() },
                |cx| {
                    let body = cx.new(|cx| DetailsBody {
                        conversation: conversation.downgrade(),
                        _changes: cx.observe(&conversation, |_, _, cx| cx.notify()),
                    });
                    opened = Some(body.entity_id());
                    body
                },
                window,
                cx,
            );
            if opened.is_some() {
                let title = conversation.downgrade();
                let nested = conversation.downgrade();
                let back = conversation.downgrade();
                host.configure_conversation(
                    &panel.key,
                    move |app| {
                        title
                            .upgrade()
                            .and_then(|conversation| {
                                let view = conversation.read(app);
                                view.details
                                    .as_ref()
                                    .map(|panel| panel.title(&view.address, app))
                            })
                            .unwrap_or_else(|| {
                                Preferences::read_for(app, &owner)
                                    .language
                                    .tr("info.information")
                            })
                    },
                    move |app| {
                        let conversation = nested.upgrade()?;
                        let panel = conversation.read(app).details.as_ref()?;
                        if panel.gallery.is_some() {
                            Some(ConversationBackButton::Compact)
                        } else {
                            panel
                                .encrypted_screen()
                                .map(|_| ConversationBackButton::Standard)
                        }
                    },
                    move |app| {
                        back.update(app, |view, cx| {
                            let Some(panel) = &mut view.details else {
                                return false;
                            };
                            if let Some((_, focus)) = panel.encrypted_screens.pop() {
                                panel.refocus_details = Some(focus);
                                cx.notify();
                                return true;
                            }
                            let Some(mut gallery) = panel.gallery.take() else {
                                return false;
                            };
                            panel.refocus_details =
                                Some(gallery.trigger(&panel.gallery_triggers).clone());
                            gallery.release_previews(cx);
                            view.cancel_gallery_export();
                            cx.notify();
                            true
                        })
                        .unwrap_or(false)
                    },
                );
            }
        });
        if let Some(id) = opened {
            panel.body = Some(id);
            self.heading_request.take();
            self.refresh_gallery_counts(cx);
            self.request_details(cx);
        }
    }

    /// После перехода ждёт подтверждения доступа к выбранной теме.
    pub(in crate::gpui_client) fn restore_open_details(&mut self, cx: &mut Context<Self>) {
        if let Some(panel) = &mut self.details {
            panel.reopen_on_access = true;
            cx.notify();
        }
    }

    /// Открывает перенесённую панель после подтверждения истории темы.
    pub(super) fn restore_details_for_verified_history(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_access()
            && self
                .details
                .as_mut()
                .is_some_and(|panel| std::mem::take(&mut panel.reopen_on_access))
        {
            self.load_details(false, window, cx);
        }
    }

    /// Читает присутствие и состав через тот же кэш Core, который использует панель.
    pub(super) fn refresh_heading_details(&mut self, cx: &mut Context<Self>) {
        if !self.can_open_details() {
            return;
        }
        let Some(panel) = &self.details else {
            return;
        };
        if panel.is_present(cx) {
            self.heading_request.take();
            return;
        }
        let runtime = panel.runtime.clone();
        let address = self.address.clone();
        self.heading_request = Some(cx.spawn(async move |view, cx| {
            let reply = source::load(runtime, address).await;
            let _ = view.update(cx, |view, cx| {
                view.heading_request.take();
                if view.can_open_details()
                    && let Some(result) = reply.for_current(Some(&view.address))
                {
                    view.heading_details = result.ok();
                    cx.notify();
                }
            });
        }));
    }

    /// Обновляет открытый снимок после события изменений того же стрима.
    pub(super) fn refresh_open_details(&mut self, cx: &mut Context<Self>) {
        if self
            .details
            .as_ref()
            .is_some_and(|panel| panel.is_present(cx))
        {
            self.request_details(cx);
        }
    }

    /// Читает снимок из core, сверяя адрес и последнее намерение перед показом.
    fn request_details(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.details else {
            return;
        };
        let op_id = Uuid::new_v4();
        let runtime = panel.runtime.clone();
        let address = self.address.clone();
        log::debug!("gpui_details op_id={op_id} event=request_started");
        let task = cx.spawn(async move |view, cx| {
            let reply = source::load(runtime, address).await;
            let _ = view.update(cx, |view, cx| {
                let Some(panel) = &mut view.details else {
                    return;
                };
                if !panel.is_present(cx) || !matches!(panel.state, Some(State::Loading { op_id: current, .. }) if current == op_id) {
                    log::debug!("gpui_details op_id={op_id} event=stale_request");
                    return;
                }
                let Some(result) = reply.for_current(Some(&view.address)) else {
                    log::debug!("gpui_details op_id={op_id} event=stale_address");
                    return;
                };
                log::debug!("gpui_details op_id={op_id} event=request_completed success={}", result.is_ok());
                let previous_title = panel.title(&view.address, cx);
                match result {
                    Ok(details) => {
                        if !matches!(details.as_ref(), SidebarDetails::Channel(channel) if channel.can_add_members) {
                            panel.picker = None;
                        }
                        let encrypted = matches!(details.as_ref(), SidebarDetails::Channel(channel) if channel.encrypted);
                        if !encrypted {
                            panel.clear_encrypted_screens();
                        }
                        panel.refresh_avatars(&details, &view.address, cx);
                        panel.topic_menu.sync(&details, cx);
                        panel.sync_summary(&details, view.address.route.scope.topic_id);
                        view.heading_details = Some(details.clone());
                        panel.snapshot = Some(details);
                        panel.state = Some(State::Ready);
                        if encrypted {
                            view.load_key_management(cx);
                        }
                    }
                    Err(Problem::AccessLost) => view.fail(Failure::AccessLost, cx),
                    Err(error) => {
                        view.heading_details = None;
                        panel.snapshot = None;
                        panel.clear_encrypted_screens();
                        panel.summary_content = None;
                        panel.avatars.clear();
                        panel.picker = None;
                        panel.state = Some(State::Failed(error));
                    }
                }
                if let Some(panel) = view.details.as_ref()
                    && panel.is_present(cx)
                    && panel.title(&view.address, cx) != previous_title
                {
                    panel.refresh_frame(cx);
                }
                cx.notify();
            });
        });
        panel.state = Some(State::Loading {
            op_id,
            _request: task,
        });
        cx.notify();
    }

    /// Выполняет доступное действие только по снимку этого разговора.
    fn handle_panel_action(
        &mut self,
        intent: PanelAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = &mut self.details else {
            return;
        };
        if !panel.host.read(cx).is_active(&panel.key) {
            return;
        }
        if let PanelAction::Gallery(action) = intent {
            self.handle_gallery_action(action, window, cx);
            return;
        }
        match (intent, panel.snapshot.as_deref()) {
            (PanelAction::MessagePeer, Some(SidebarDetails::Peer(_))) => {
                if let Some(sending) = &self.sending {
                    panel.reset(cx);
                    panel.host.update(cx, |host, cx| host.pop(window, cx));
                    sending.view.read(cx).focus_editor(window, cx);
                    cx.notify();
                }
            }
            (PanelAction::Copy { field, value }, Some(SidebarDetails::Peer(_))) => {
                cx.write_to_clipboard(ClipboardItem::new_string(value));
                let feedback = cx.background_executor().timer(Duration::from_secs(2));
                panel.copy_feedback.insert(
                    field,
                    cx.spawn(async move |view, cx| {
                        feedback.await;
                        let _ = view.update(cx, |view, cx| {
                            if let Some(panel) = &mut view.details {
                                panel.copy_feedback.remove(&field);
                                cx.notify();
                            }
                        });
                    }),
                );
                cx.notify();
            }
            (PanelAction::CallPeer, Some(SidebarDetails::Peer(peer))) if !peer.is_self => {
                self.start_call(cx);
            }
            (action @ (PanelAction::Favorites | PanelAction::EditPeer), Some(SidebarDetails::Peer(peer)))
                if peer.is_self =>
            {
                cx.emit(DetailsProfileIntent {
                    address: self.address.clone(),
                    action: match action {
                        PanelAction::Favorites => DetailsProfileAction::Favorites,
                        _ => DetailsProfileAction::Edit,
                    },
                    opener: window.focused(cx).unwrap_or_else(|| panel.trigger.clone()),
                });
            }
            (PanelAction::SharePeer, Some(SidebarDetails::Peer(peer))) => {
                let Ok(link) = sidebar_profile_link(&self.address.route.owner, peer.user_id) else {
                    return;
                };
                cx.write_to_clipboard(ClipboardItem::new_string(link));
                let feedback = cx.background_executor().timer(Duration::from_secs(2));
                panel.share_feedback = Some(cx.spawn(async move |view, cx| {
                    feedback.await;
                    let _ = view.update(cx, |view, cx| {
                        if let Some(panel) = &mut view.details {
                            panel.share_feedback = None;
                            cx.notify();
                        }
                    });
                }));
                cx.notify();
            }
            (PanelAction::ToggleTopics, Some(SidebarDetails::Channel(_))) => {
                panel.topics_expanded = !panel.topics_expanded;
                cx.notify();
            }
            (PanelAction::ToggleSummary, Some(SidebarDetails::Channel(channel))) => {
                if let Some(topic) = channel
                    .topics
                    .iter()
                    .find(|topic| Some(topic.id) == self.address.route.scope.topic_id)
                {
                    let expanded = panel.summary_expanded.unwrap_or(topic.summary.is_some());
                    panel.summary_expanded = Some(!expanded);
                    cx.notify();
                }
            }
            (PanelAction::OpenSummarySettings, Some(SidebarDetails::Channel(_))) => {
                self.open_summary_settings(window, cx);
            }
            (PanelAction::OpenTopic(topic_id), Some(SidebarDetails::Channel(channel)))
                if channel.topics.iter().any(|topic| topic.id == topic_id) =>
            {
                cx.emit(OpenDetailsTopic {
                    address: self.address.clone(),
                    topic_id,
                });
            }
            (PanelAction::OpenTopicMenu { topic, anchor }, Some(SidebarDetails::Channel(_))) => {
                self.open_topic_menu(topic, anchor, window, cx);
            }
            (PanelAction::SetNotifications(level), Some(SidebarDetails::Channel(channel)))
                if channel.notification_mode != presentation::stream_mode(level) =>
            {
                self.start_sidebar_mutation(
                    Mutation::Notifications(presentation::stream_mode(level)),
                    window,
                    cx,
                );
            }
            (PanelAction::AddMembers, Some(SidebarDetails::Channel(channel)))
                if channel.can_add_members =>
            {
                self.open_member_picker(window, cx);
            }
            (PanelAction::RemoveMember(binding_id), Some(SidebarDetails::Channel(channel)))
                if channel
                    .members
                    .iter()
                    .any(|member| member.binding_id == binding_id && member.can_remove) =>
            {
                self.start_sidebar_mutation(Mutation::RemoveMember(binding_id), window, cx);
            }
            _ => {}
        }
    }

    /// Запускает одно изменение и принимает результат только в текущем открытии панели.
    fn start_sidebar_mutation(
        &mut self,
        mutation: Mutation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = &mut self.details else {
            return;
        };
        if panel.mutation.is_some() || !matches!(panel.state, Some(State::Ready)) {
            return;
        }
        let kind = match &mutation {
            Mutation::Notifications(_) => MutationKind::Notifications,
            Mutation::AddMembers(_) => MutationKind::Add,
            Mutation::RemoveMember(_) => MutationKind::Remove,
            Mutation::Topic(..) => MutationKind::Topic,
            Mutation::KeySharing(_) => MutationKind::KeySharing,
        };
        let id = Uuid::new_v4();
        let address = self.address.clone();
        let runtime = panel.runtime.clone();
        panel.mutation = Some(PendingMutation { id });
        panel.mutation_error = None;
        log::debug!("gpui_details op_id={id} event=mutation_started");
        cx.spawn_in(window, async move |view, cx| {
            let reply = source::mutate(runtime, address, mutation).await;
            let _ = cx.update(|window, app| {
                let _ = view.update(app, |view, cx| {
                    let Some(panel) = &mut view.details else {
                        return;
                    };
                    if !panel.is_present(cx)
                        || !panel
                            .mutation
                            .as_ref()
                            .is_some_and(|current| current.id == id)
                    {
                        return;
                    }
                    let Some(result) = reply.for_current(Some(&view.address)) else {
                        return;
                    };
                    panel.mutation = None;
                    log::debug!(
                        "gpui_details op_id={id} event=mutation_completed success={}",
                        result.is_ok()
                    );
                    match result {
                        Ok(()) => {
                            if kind == MutationKind::Add && panel.picker.is_some() {
                                view.close_member_picker(window, cx);
                            }
                            cx.notify();
                        }
                        Err(Problem::AccessLost) => view.fail(Failure::AccessLost, cx),
                        Err(_) => {
                            panel.mutation_error = Some(kind);
                            cx.notify();
                        }
                    }
                });
            });
        })
        .detach();
        cx.notify();
    }
}

/// Передаёт нажатие живому экрану разговора вместе с системным окном.
fn action(
    cx: &Context<Conversation>,
    handle: impl Fn(&mut Conversation, &mut Window, &mut Context<Conversation>) + 'static,
) -> impl Fn(&mut Window, &mut App) + 'static {
    let target = cx.entity().downgrade();
    move |window, app| {
        let _ = target.update(app, |view, cx| handle(view, window, cx));
    }
}

#[cfg(all(test, feature = "gpui-test"))]
impl Conversation {
    /// Проверяет текущую видимость. GPUI сохраняет старые debug bounds удалённых элементов.
    pub(in crate::gpui_client) fn tested_details_is_open(&self, app: &App) -> bool {
        self.details_active(app)
    }

    /// Возвращает текущий экран защищённых сведений по живому состоянию панели.
    pub(in crate::gpui_client) fn tested_details_screen(&self, app: &App) -> Option<&'static str> {
        let panel = self.details.as_ref().filter(|_| self.details_active(app))?;
        if panel.gallery.is_some() {
            return Some("gallery");
        }
        match panel.snapshot.as_deref()? {
            SidebarDetails::Channel(channel) if channel.encrypted => {
                Some(match panel.encrypted_screen() {
                    Some(EncryptedScreen::Settings) => "settings",
                    Some(EncryptedScreen::Keys) => "keys",
                    Some(EncryptedScreen::LocalKey(_)) => "local-key",
                    None => "info",
                })
            }
            _ => None,
        }
    }

    /// Возвращает последнее явное раскрытие контекста в интерфейсном тесте.
    pub(in crate::gpui_client) fn tested_details_summary_expanded(&self) -> Option<bool> {
        self.details.as_ref()?.summary_expanded
    }

    /// Возвращает снимок, из которого сейчас построена видимая карточка.
    pub(in crate::gpui_client) fn tested_details<R>(
        &self,
        app: &App,
        inspect: impl FnOnce(Option<&SidebarDetails>) -> R,
    ) -> R {
        let details = self.details.as_ref().filter(|panel| panel.is_present(app));
        inspect(details.and_then(|panel| panel.snapshot.as_deref()))
    }

    /// Возвращает идентификатор изображения для проверки замены фото после события.
    pub(in crate::gpui_client) fn tested_details_avatar(&self, app: &App) -> Option<gpui::ImageId> {
        self.tested_details(app, |details| {
            let Some(SidebarDetails::Peer(peer)) = details else {
                return None;
            };
            let gpui::ImageSource::Render(image) =
                self.details.as_ref()?.avatars.image(peer.user_id, app)?
            else {
                return None;
            };
            Some(image.id)
        })
    }

    /// Возвращает координату строки из текущего кадра для замера открытия панели.
    pub(in crate::gpui_client) fn tested_details_row_y(&self, id: Uuid) -> Option<f32> {
        self.viewport.y(id)
    }

    /// Показывает, какая кнопка сведений сейчас владеет фокусом.
    pub(in crate::gpui_client) fn tested_details_focus(
        &self,
        window: &Window,
        app: &App,
    ) -> Option<&'static str> {
        let panel = self.details.as_ref()?;
        if panel.trigger.is_focused(window) {
            Some("сведения")
        } else if panel.title_trigger.is_focused(window) {
            Some("заголовок")
        } else if panel.host.read(app).close_focus(&panel.key, window) {
            Some("закрыть")
        } else if panel.retry.is_focused(window) {
            Some("повторить")
        } else {
            None
        }
    }
}
