//! Управляет выбранным аккаунтом, счётчиками и маршрутом окна.
//! Действия окна и события рабочего потока изменяют состояние и возвращают команды для клиента GPUI.

use std::{collections::HashMap, sync::Arc};
use uuid::Uuid;
use workspace_core::{
    auth::SessionStatus,
    bootstrap::{ChatList, ChatLists},
    context::{AccountId, OwnerContext, SessionGeneration},
    messenger::{FolderSnapshot, catalog::Chat},
    notification_target::NotificationTarget,
    storage::HistoryScope,
    sync::{SyncChange, SyncUpdate},
};

use super::contracts::{
    AccountSession, AccountSnapshot, ChatEntry, ChatRoute, ConversationAddress, NavigationRevision,
    NotificationReply, NotificationRequest, NotificationRouteError, RuntimeEvent, ShellAction,
    SidebarAction,
};
use crate::platform::notifications::NotificationClick;

/// Команда, которую клиент выполняет после изменения состояния оболочки.
pub enum ShellEffect {
    OpenConversation(ConversationAddress), // Открыть историю по адресу нового перехода.
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

    /// Возвращает адрес открытой истории для проверки поздних ответов.
    pub fn conversation(&self) -> Option<&ConversationAddress> {
        self.conversation.as_ref()
    }

    /// Даёт оболочке безопасный текст ошибки цели уведомления.
    pub fn route_error(&self) -> Option<&'static str> {
        Some(match self.route_error? {
            NotificationRouteError::InactiveSession => "Аккаунт из уведомления недоступен",
            NotificationRouteError::TargetUnavailable => "Сообщение из уведомления недоступно",
            NotificationRouteError::LookupFailed => "Не удалось открыть уведомление",
        })
    }

    /// Обрабатывает действие пользователя из переключателя, каталога или системного меню.
    pub fn handle_action(&mut self, action: ShellAction) -> Option<ShellEffect> {
        match action {
            ShellAction::SelectAccount(session) => self.select(session),
            ShellAction::Sidebar(SidebarAction::OpenChat { route, generation }) => {
                self.open_sidebar_route(route, generation)
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
        let invalid_conversation = self.conversation.is_some()
            && self
                .selected()
                .is_none_or(|session| !self.is_active(session));
        if (previous.is_some() && previous != self.selected) || invalid_conversation {
            self.begin_navigation();
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

    /// Повторяет сохранённый переход, только если разговор ещё в каталоге текущего входа.
    pub(super) fn restore_navigation(
        &mut self,
        route: Option<&ChatRoute>,
    ) -> Result<Option<ShellEffect>, ()> {
        let selected = self
            .selected()
            .filter(|session| self.is_active(session))
            .ok_or(())?;
        if self.conversation.as_ref().map(|address| &address.route) == route {
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
        Ok(match route {
            Some(route) => Some(self.open_conversation(route.clone(), generation)),
            None => self
                .conversation
                .take()
                .map(|_| ShellEffect::CloseConversation),
        })
    }

    /// Открывает точную тему, которую Core подтвердил при создании защищённого канала.
    pub(super) fn open_created_encrypted_channel(
        &mut self,
        session: &AccountSession,
        stream_id: Uuid,
        topic_id: Uuid,
    ) -> Option<ShellEffect> {
        if self.selected() != Some(session)
            || !self.is_active(session)
            || stream_id.is_nil()
            || topic_id.is_nil()
        {
            return None;
        }
        self.begin_navigation();
        Some(self.open_conversation(
            ChatRoute {
                owner: session.owner.clone(),
                scope: HistoryScope {
                    stream_id,
                    topic_id: Some(topic_id),
                },
                entry: ChatEntry::Latest,
            },
            session.generation,
        ))
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
        self.conversation = Some(address.clone());
        log::debug!(
            "gpui_shell op_id={} event=conversation_opened",
            self.revision.0
        );
        ShellEffect::OpenConversation(address)
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

/// Проверяет, что каталог содержит личный чат или тему её канала.
fn catalog_contains(chats: &[Chat], scope: HistoryScope) -> bool {
    chats.iter().any(|chat| {
        chat.id == scope.stream_id
            && match scope.topic_id {
                None => chat.direct,
                Some(topic_id) => {
                    !chat.direct && chat.topics.iter().any(|topic| topic.id == topic_id)
                }
            }
    })
}

#[cfg(test)]
mod tests;
