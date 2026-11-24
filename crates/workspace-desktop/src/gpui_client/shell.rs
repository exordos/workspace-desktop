//! Управляет выбранным аккаунтом, счётчиками и маршрутом окна.
//! Действия окна и события рабочего потока изменяют состояние и возвращают команды для клиента GPUI.

use std::{collections::HashMap, sync::Arc};
use uuid::Uuid;
use workspace_core::{
    auth::SessionStatus,
    bootstrap::{ChatList, ChatLists},
    context::{AccountId, OwnerContext, SessionGeneration},
    messenger::{FolderSnapshot, activity::ActivityMessage, catalog::Chat},
    notification_target::NotificationTarget,
    storage::HistoryScope,
    sync::{SyncChange, SyncUpdate},
};

use super::contracts::{
    AccountSession, AccountSnapshot, ActivityAddress, ActivityPage, ChatEntry, ChatRoute,
    ConversationAddress, NavigationRevision, NotificationReply, NotificationRequest,
    NotificationRouteError, RuntimeEvent, ShellAction, SidebarAction,
};
use crate::platform::notifications::NotificationClick;

/// Команда, которую клиент выполняет после изменения состояния оболочки.
pub enum ShellEffect {
    OpenConversation(ConversationAddress), // Открыть историю по адресу нового перехода.
    OpenDraft(ConversationAddress, Uuid),  // Открыть разговор с конкретным черновиком редактора.
    CloseConversation,                     // Снять экран истории при смене видимого входа.
    RetryCatalog(AccountSession),          // Повторить загрузку каталога указанного входа.
    ResolveNotification(NotificationRequest), // Проверить цель на рабочем потоке.
    ShowNotification(NotificationClick),   // Попросить ОС показать баннер без текста сообщения.
    ReloadConversation(ConversationAddress), // Перечитать историю после пропуска событий.
    ConversationUpdated(ConversationAddress, Box<SyncUpdate>), // Передать событие владельцу экрана истории.
}

/// Хранит состояние окна независимо от фонового исполнителя и разметки GPUI.
#[derive(Default)]
pub struct Shell {
    accounts: Vec<AccountSnapshot>, // Последний снимок доступных аккаунтов и их счётчиков.
    catalogs: ChatLists,            // Каталоги всех аккаунтов, полученные от Bootstrap.
    folders: HashMap<OwnerContext, Arc<FolderSnapshot>>, // Согласованный снимок, из которого видны строки папок.
    preferred: Option<AccountId>, // Аккаунт, который по возможности нужно выбрать при запуске.
    selected: Option<AccountSession>, // Вход, чей каталог и экран сейчас показаны.
    conversation: Option<ConversationAddress>, // Открытая история, если выбран разговор.
    conversation_draft: Option<Uuid>, // Точный черновик, если разговор открыт с его карточки.
    activity: Option<ActivityAddress>, // Выбранный раздел активности вместо истории.
    revision: NavigationRevision, // Последнее принятое намерение о переходе.
    pending_notification: Option<NotificationRequest>, // Клик, ответ на который окно ещё ждёт.
    route_error: Option<NotificationRouteError>, // Ошибка текущей цели уведомления для показа в окне.
}

impl Shell {
    /// Готовит оболочку до прихода первого снимка аккаунтов.
    pub fn new(preferred: Option<AccountId>) -> Self {
        Self {
            preferred,
            ..Self::default()
        }
    }

    /// Возвращает снимки для переключателя, включая фоновые счётчики.
    pub fn accounts(&self) -> &[AccountSnapshot] {
        &self.accounts
    }

    /// Возвращает вход, которому принадлежит видимый контекст.
    pub fn selected(&self) -> Option<&AccountSession> {
        self.selected.as_ref()
    }

    /// Возвращает каталог только для поколения выбранного активного входа.
    pub fn selected_catalog(&self) -> Option<&ChatList> {
        let session = self.selected()?;
        if !self.is_active(session) {
            return None;
        }
        self.catalogs
            .get(&session.owner)
            .filter(|catalog| catalog.generation == session.generation)
    }

    /// Использует тот же резервный снимок папок, который доступен каталогу чатов.
    pub(super) fn selected_activity_chats(&self) -> Option<&[Chat]> {
        let session = self.selected().filter(|session| self.is_active(session))?;
        session.catalog_chats(
            self.selected_catalog(),
            self.folders.get(&session.owner).map(Arc::as_ref),
        )
    }

    /// Возвращает адрес открытой истории для проверки поздних ответов.
    pub fn conversation(&self) -> Option<&ConversationAddress> {
        self.conversation.as_ref()
    }

    /// Возвращает точный черновик открытого разговора для истории переходов.
    pub(super) fn conversation_draft(&self) -> Option<Uuid> {
        self.conversation_draft
    }

    /// Возвращает адрес страницы «Моей активности» в рабочей области.
    pub(super) fn activity(&self) -> Option<&ActivityAddress> {
        self.activity.as_ref()
    }

    /// Возвращает безопасную категорию ошибки цели уведомления для перевода в окне.
    pub fn route_error(&self) -> Option<NotificationRouteError> {
        self.route_error
    }

    /// Обрабатывает действие пользователя из переключателя, каталога или системного меню.
    pub fn handle_action(&mut self, action: ShellAction) -> Option<ShellEffect> {
        match action {
            ShellAction::SelectAccount(session) => self.select(session),
            ShellAction::Sidebar(SidebarAction::OpenChat { route, generation }) => {
                self.open_sidebar_route(route, generation)
            }
            ShellAction::Sidebar(SidebarAction::OpenCreatedTopic { session, stream_id, topic }) => {
                self.open_created_topic(&session, stream_id, topic)
            }
            ShellAction::Sidebar(SidebarAction::OpenActivity { page, session }) => {
                self.open_activity(page, session)
            }
            ShellAction::Sidebar(SidebarAction::RetryCatalog(session)) => {
                (self.selected() == Some(&session) && self.is_active(&session))
                    .then_some(ShellEffect::RetryCatalog(session))
            }
            ShellAction::NotificationClicked(click) => self.request_notification(click),
        }
    }

    /// Применяет снимки и обновления после их доставки на поток окна.
    pub fn handle_runtime(&mut self, event: RuntimeEvent) -> Option<ShellEffect> {
        match event {
            RuntimeEvent::Accounts(accounts) => self.replace_accounts(accounts),
            RuntimeEvent::Catalog(catalogs) => {
                self.catalogs = catalogs;
                None
            }
            RuntimeEvent::Folders(folders) => {
                self.folders = folders;
                None
            }
            RuntimeEvent::Sync(update) => self.apply_sync(update),
            RuntimeEvent::SyncLagged => self
                .conversation()
                .cloned()
                .map(ShellEffect::ReloadConversation),
            RuntimeEvent::FolderPreferences(_) | RuntimeEvent::Stopped(_) => None,
        }
    }

    /// Открывает проверенную цель или показывает причину отказа, если клик ещё актуален.
    pub fn handle_notification_reply(&mut self, reply: NotificationReply) -> Option<ShellEffect> {
        let request = &reply.request;
        if self.pending_notification.as_ref().is_none_or(|pending| {
            pending.revision != request.revision || pending.click != request.click
        }) {
            log::debug!(
                "gpui_shell op_id={} event=notification_reply outcome=stale",
                request.revision.0
            );
            return None;
        }
        self.pending_notification = None;
        let session = notification_session(&request.click);
        if !self.is_active(&session) {
            return None;
        }
        let route = match reply.result {
            Ok(route) if matches_notification(&route, &request.click.target) => route,
            Ok(_) => {
                self.reject_notification(NotificationRouteError::TargetUnavailable);
                return None;
            }
            Err(error) => {
                self.reject_notification(error);
                return None;
            }
        };
        self.selected = Some(session);
        self.preferred = None;
        Some(self.open_conversation(route, request.click.generation))
    }

    /// Заменяет снимок и закрывает историю, если показанный вход исчез или потерял доступ.
    fn replace_accounts(&mut self, accounts: Vec<AccountSnapshot>) -> Option<ShellEffect> {
        let previous = self.selected.clone();
        self.accounts = accounts;
        let preferred = self.preferred.as_ref().and_then(|preferred| {
            self.accounts
                .iter()
                .find(|account| &account.session.owner.account_id == preferred)
        });
        let selected = self.selected.as_ref().and_then(|selected| {
            self.accounts
                .iter()
                .find(|account| account.session == *selected)
                .or_else(|| {
                    self.accounts.iter().find(|account| {
                        account.session.owner.account_id == selected.owner.account_id
                    })
                })
        });
        if preferred.is_some() {
            self.preferred = None;
        }
        self.selected = preferred
            .or(selected)
            .or_else(|| self.accounts.first())
            .map(|account| account.session.clone());
        if let Some(pending) = self.pending_notification.as_ref()
            && !self.is_active(&notification_session(&pending.click))
        {
            // Ответ завершённого входа не должен показывать ошибку поверх нового входа.
            self.pending_notification = None;
        }
        let invalid_page = (self.conversation.is_some() || self.activity.is_some())
            && self
                .selected()
                .is_none_or(|session| !self.is_active(session));
        if (previous.is_some() && previous != self.selected) || invalid_page {
            self.begin_navigation();
            self.activity = None;
            self.conversation_draft = None;
            return self
                .conversation
                .take()
                .map(|_| ShellEffect::CloseConversation);
        }
        None
    }

    /// Сохраняет явный выбор, если строка аккаунта всё ещё относится к текущему снимку.
    fn select(&mut self, session: AccountSession) -> Option<ShellEffect> {
        if !self
            .accounts
            .iter()
            .any(|account| account.session == session)
        {
            return None;
        }
        self.begin_navigation();
        self.preferred = None;
        if self.selected() == Some(&session) {
            return None;
        }
        self.selected = Some(session);
        self.activity = None;
        self.conversation_draft = None;
        log::debug!(
            "gpui_shell op_id={} event=account_selected",
            self.revision.0
        );
        self.conversation
            .take()
            .map(|_| ShellEffect::CloseConversation)
    }

    /// Открывает последние сообщения только для строки актуального каталога.
    fn open_sidebar_route(
        &mut self,
        route: ChatRoute,
        generation: SessionGeneration,
    ) -> Option<ShellEffect> {
        let selected = self.selected()?;
        if selected.owner != route.owner
            || selected.generation != generation
            || !self.is_active(selected)
            || route.entry != ChatEntry::Latest
        {
            return None;
        }
        let chats = selected.catalog_chats(
            self.selected_catalog(),
            self.folders.get(&route.owner).map(Arc::as_ref),
        )?;
        if !catalog_contains(chats, route.scope) {
            return None;
        }
        self.begin_navigation();
        Some(self.open_conversation(route, generation))
    }

    /// Открывает раздел активности только для выбранного действующего входа.
    fn open_activity(
        &mut self,
        page: ActivityPage,
        session: AccountSession,
    ) -> Option<ShellEffect> {
        if self.selected() != Some(&session) || !self.is_active(&session) {
            return None;
        }
        if self
            .activity()
            .is_some_and(|address| address.page == page && address.revision == self.revision)
            && self.pending_notification.is_none()
        {
            return None;
        }
        self.begin_navigation();
        self.activity = Some(ActivityAddress {
            page,
            session,
            revision: self.revision,
        });
        self.conversation_draft = None;
        log::debug!(
            "gpui_shell op_id={} event=activity_opened page={page:?}",
            self.revision.0
        );
        self.conversation
            .take()
            .map(|_| ShellEffect::CloseConversation)
    }

    /// Открывает строку страницы, если её показ и цель в каталоге всё ещё актуальны.
    pub(super) fn open_activity_target(
        &mut self,
        source: &ActivityAddress,
        scope: HistoryScope,
        entry: ChatEntry,
    ) -> Option<ShellEffect> {
        if self.activity() != Some(source) || !self.is_active(&source.session) {
            return None;
        }
        let chats = source.session.catalog_chats(
            self.selected_catalog(),
            self.folders.get(&source.session.owner).map(Arc::as_ref),
        )?;
        if !catalog_contains(chats, scope) {
            return None;
        }
        let route = ChatRoute {
            owner: source.session.owner.clone(),
            scope,
            entry,
        };
        self.begin_navigation();
        Some(self.open_conversation(route, source.session.generation))
    }

    /// Использует тот же переход из списка, сохраняя точный UUID черновика для редактора.
    pub(super) fn open_activity_draft(
        &mut self,
        source: &ActivityAddress,
        scope: HistoryScope,
        draft_id: Uuid,
    ) -> Option<ShellEffect> {
        if source.page != ActivityPage::Drafts || draft_id.is_nil() {
            return None;
        }
        let effect = self.open_activity_target(source, scope, ChatEntry::Latest)?;
        Some(self.effect_with_draft(effect, Some(draft_id)))
    }

    /// Передаёт UUID редактору при открытии или восстановлении перехода из списка черновиков.
    fn effect_with_draft(&mut self, effect: ShellEffect, draft: Option<Uuid>) -> ShellEffect {
        match (effect, draft) {
            (ShellEffect::OpenConversation(address), Some(id)) => {
                self.conversation_draft = Some(id);
                ShellEffect::OpenDraft(address, id)
            }
            (effect, _) => effect,
        }
    }

    /// Использует проверенный сервером адрес карточки, даже пока каталог не загружен.
    pub(super) fn open_activity_message(
        &mut self,
        source: &ActivityAddress,
        message: &ActivityMessage,
    ) -> Option<ShellEffect> {
        if self.activity() != Some(source)
            || self.selected() != Some(&source.session)
            || !self.is_active(&source.session)
            || !matches!(source.page, ActivityPage::Marked | ActivityPage::Mentions)
            || message.id.is_nil()
            || message.stream_id.is_nil()
            || message.topic_id.is_nil()
        {
            return None;
        }
        let route = ChatRoute {
            owner: source.session.owner.clone(),
            scope: HistoryScope {
                stream_id: message.stream_id,
                topic_id: Some(message.topic_id),
            },
            entry: ChatEntry::Around(message.id),
        };
        self.begin_navigation();
        Some(self.open_conversation(route, source.session.generation))
    }

    /// Проверяет поколение и намерение перед применением отложенного перехода.
    pub(super) fn can_finish_activity(&self, source: &ActivityAddress) -> bool {
        self.activity() == Some(source)
            && self.selected() == Some(&source.session)
            && self.is_active(&source.session)
            && self.revision == source.revision
    }

    /// Открывает проверенный Core личный чат, в том числе до обновления каталога.
    pub(super) fn open_favorites_target(
        &mut self,
        source: &ActivityAddress,
        scope: HistoryScope,
    ) -> Option<ShellEffect> {
        if source.page != ActivityPage::Favorites
            || !self.can_finish_activity(source)
            || scope.stream_id.is_nil()
            || scope.topic_id.is_some()
        {
            return None;
        }
        self.begin_navigation();
        let effect = self.open_conversation(
            ChatRoute {
                owner: source.session.owner.clone(),
                scope,
                entry: ChatEntry::Latest,
            },
            source.session.generation,
        );
        self.activity = Some(ActivityAddress {
            revision: self.revision,
            ..source.clone()
        });
        Some(effect)
    }

    /// Повторяет сохранённый переход, только если разговор ещё в каталоге текущего входа.
    pub(super) fn restore_navigation(
        &mut self,
        route: Option<&ChatRoute>,
        activity: Option<ActivityPage>,
        draft: Option<Uuid>,
    ) -> Result<Option<ShellEffect>, ()> {
        let selected = self
            .selected()
            .filter(|session| self.is_active(session))
            .ok_or(())?;
        if let Some(page) = activity {
            return Ok(self.open_activity(page, selected.clone()));
        }
        if (draft.is_some()
            || self.activity.as_ref().is_none_or(|address| {
                address.page == ActivityPage::Favorites && route.is_some()
            }))
            && self.conversation.as_ref().map(|address| &address.route) == route
            && self.conversation_draft == draft
        {
            return Ok(None);
        }
        let generation = selected.generation;
        if let Some(route) = route {
            let chats = selected
                .catalog_chats(
                    self.selected_catalog(),
                    self.folders.get(&route.owner).map(Arc::as_ref),
                )
                .ok_or(())?;
            if selected.owner != route.owner || !catalog_contains(chats, route.scope) {
                return Err(());
            }
        }
        self.begin_navigation();
        self.activity = None;
        self.conversation_draft = None;
        Ok(match route {
            Some(route) => {
                let effect = self.open_conversation(route.clone(), generation);
                Some(self.effect_with_draft(effect, draft))
            }
            None => self
                .conversation
                .take()
                .map(|_| ShellEffect::CloseConversation),
        })
    }

    /// Открывает точную тему, которую Core подтвердил при создании темы или канала.
    pub(super) fn open_created_topic(
        &mut self, session: &AccountSession, stream_id: Uuid, topic_id: Uuid,
    ) -> Option<ShellEffect> {
        if self.selected() != Some(session) || !self.is_active(session) || stream_id.is_nil() || topic_id.is_nil() {
            return None;
        }
        self.begin_navigation();
        self.activity = None;
        self.conversation_draft = None;
        Some(self.open_conversation(ChatRoute {
            owner: session.owner.clone(),
            scope: HistoryScope { stream_id, topic_id: Some(topic_id) },
            entry: ChatEntry::Latest,
        }, session.generation))
    }

    /// Открывает оригинал пересылки из того же экрана и активного входа.
    pub(super) fn open_forward_original(
        &mut self,
        source: &ConversationAddress,
        scope: HistoryScope,
        id: Uuid,
    ) -> Option<ShellEffect> {
        let session = self.selected()?;
        if self.conversation() != Some(source)
            || session.owner != source.route.owner
            || session.generation != source.generation
            || !self.is_active(session)
        {
            return None;
        }
        let route = ChatRoute {
            owner: source.route.owner.clone(),
            scope,
            entry: ChatEntry::Around(id),
        };
        self.begin_navigation();
        Some(self.open_conversation(route, source.generation))
    }

    /// Открывает тему из проверенного снимка панели текущего разговора.
    pub(super) fn open_details_topic(
        &mut self,
        source: &ConversationAddress,
        topic_id: Uuid,
    ) -> Option<ShellEffect> {
        let session = self.selected()?;
        if self.conversation() != Some(source)
            || session.owner != source.route.owner
            || session.generation != source.generation
            || !self.is_active(session)
            || source.route.scope.topic_id == Some(topic_id)
        {
            return None;
        }
        let mut route = source.route.clone();
        route.scope.topic_id = Some(topic_id);
        route.entry = ChatEntry::Latest;
        self.begin_navigation();
        Some(self.open_conversation(route, source.generation))
    }

    /// Начинает проверку баннера, сохраняя видимый разговор до получения ответа.
    fn request_notification(&mut self, click: NotificationClick) -> Option<ShellEffect> {
        self.begin_navigation();
        if !self.is_active(&notification_session(&click)) {
            self.reject_notification(NotificationRouteError::InactiveSession);
            return None;
        }
        let request = NotificationRequest {
            revision: self.revision,
            click,
        };
        self.pending_notification = Some(request.clone());
        log::debug!(
            "gpui_shell op_id={} event=notification_requested",
            self.revision.0
        );
        Some(ShellEffect::ResolveNotification(request))
    }

    /// Записывает ошибку без смены аккаунта или адреса текущей истории.
    fn reject_notification(&mut self, error: NotificationRouteError) {
        self.route_error = Some(error);
        log::debug!(
            "gpui_shell op_id={} event=notification_rejected reason={error:?}",
            self.revision.0
        );
    }

    /// Сохраняет адрес, одинаковый для переходов из каталога и уведомления.
    fn open_conversation(
        &mut self,
        route: ChatRoute,
        generation: SessionGeneration,
    ) -> ShellEffect {
        let address = ConversationAddress {
            route,
            generation,
            revision: self.revision,
        };
        self.activity = self
            .favorites_session(&address)
            .map(|session| ActivityAddress {
                page: ActivityPage::Favorites,
                session: session.clone(),
                revision: address.revision,
            });
        self.conversation_draft = None;
        self.conversation = Some(address.clone());
        log::debug!(
            "gpui_shell op_id={} event=conversation_opened",
            self.revision.0
        );
        ShellEffect::OpenConversation(address)
    }

    /// Узнаёт чат с собой при открытии обычного маршрута из каталога или уведомления.
    fn favorites_session(&self, address: &ConversationAddress) -> Option<&AccountSession> {
        let session = self.selected()?;
        if address.generation != session.generation || address.route.owner != session.owner {
            return None;
        }
        let user = session.user_uuid()?;
        let chats = session.catalog_chats(
            self.selected_catalog(),
            self.folders.get(&session.owner).map(Arc::as_ref),
        )?;
        chats
            .iter()
            .any(|chat| {
                chat.id == address.route.scope.stream_id
                    && chat.direct
                    && chat.metadata.direct_peer_id == Some(user)
            })
            .then_some(session)
    }

    /// Меняет номер перехода, отменяя ожидаемый ответ на прежний клик.
    fn begin_navigation(&mut self) {
        self.revision.0 += 1;
        self.pending_notification = None;
        self.route_error = None;
    }

    /// Проверяет полную адресацию и готовность входа к операциям мессенджера.
    fn is_active(&self, session: &AccountSession) -> bool {
        self.accounts
            .iter()
            .any(|account| account.session == *session && account.status == SessionStatus::Active)
    }

    /// Обрабатывает счётчики и передаёт подходящее событие владельцу истории или адаптеру ОС.
    fn apply_sync(&mut self, update: SyncUpdate) -> Option<ShellEffect> {
        let account = self.accounts.iter_mut().find(|account| {
            account.session.owner == update.context
                && account.session.generation == update.generation
                && account.status == SessionStatus::Active
        })?;
        match &update.change {
            SyncChange::Unread(count) => {
                account.unread = Some(*count);
                return None;
            }
            SyncChange::MessengerReset => account.unread = None,
            SyncChange::IncomingDirectMessage {
                conversation_id,
                message_id,
            } => {
                return self
                    .selected()
                    .filter(|selected| selected.owner.account_id != update.context.account_id)
                    .map(|_| {
                        ShellEffect::ShowNotification(NotificationClick {
                            target: NotificationTarget {
                                context: update.context.clone(),
                                conversation_id: conversation_id.clone(),
                                message_id: message_id.clone(),
                            },
                            generation: update.generation,
                        })
                    });
            }
            _ => {}
        }
        self.conversation()
            .filter(|address| {
                address.route.owner == update.context && address.generation == update.generation
            })
            .cloned()
            .map(|address| ShellEffect::ConversationUpdated(address, Box::new(update)))
    }
}

/// Возвращает адрес входа, в котором возникло уведомление.
fn notification_session(click: &NotificationClick) -> AccountSession {
    AccountSession {
        owner: click.target.context.clone(),
        generation: click.generation,
    }
}

/// Проверяет, что ответ относится к исходной цели личного сообщения.
fn matches_notification(route: &ChatRoute, target: &NotificationTarget) -> bool {
    route.owner == target.context
        && route.scope.topic_id.is_none()
        && target.conversation_id.parse::<Uuid>() == Ok(route.scope.stream_id)
        && matches!(route.entry, ChatEntry::Around(id) if target.message_id.parse::<Uuid>() == Ok(id))
}

/// Проверяет тему в её разговоре или адрес личного чата без темы.
fn catalog_contains(chats: &[Chat], scope: HistoryScope) -> bool {
    chats.iter().any(|chat| {
        chat.id == scope.stream_id
            && match scope.topic_id {
                None => chat.direct,
                Some(topic_id) => chat.topics.iter().any(|topic| topic.id == topic_id),
            }
    })
}

#[cfg(test)]
mod tests;
