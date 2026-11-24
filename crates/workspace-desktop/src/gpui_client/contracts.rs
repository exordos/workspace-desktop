//! Описывает общую адресацию аккаунтов, маршрутов и событий GPUI.
//! Сначала идентификаторы и снимки, затем действия окна и ответы фоновой работы.

use std::{collections::HashMap, sync::Arc};

use uuid::Uuid;
use workspace_core::{
    auth::{SessionSnapshot, SessionStatus},
    bootstrap::{ChatList, ChatLists},
    context::{OwnerContext, SessionGeneration},
    messenger::{FolderSnapshot, catalog::Chat},
    notification_target::NotificationTargetError,
    storage::HistoryScope,
    sync::SyncUpdate,
};

use crate::platform::notifications::NotificationClick;

/// Указывает конкретный вход в аккаунт и проект.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct AccountSession {
    pub owner: OwnerContext, // Аккаунт и проект, которым принадлежат данные.
    pub generation: SessionGeneration, // Номер входа; старый вход не подходит после повторной авторизации.
}

impl AccountSession {
    /// Возвращает UUID самого пользователя из подтверждённого контекста входа.
    pub(super) fn user_uuid(&self) -> Option<Uuid> {
        Uuid::parse_str(&self.owner.user_id)
            .ok()
            .filter(|id| !id.is_nil())
    }

    /// Выбирает разговоры этого входа. Полный пустой каталог важнее кэша папок.
    pub(super) fn catalog_chats<'a>(
        &self,
        catalog: Option<&'a ChatList>,
        folders: Option<&'a FolderSnapshot>,
    ) -> Option<&'a [Chat]> {
        catalog
            .filter(|catalog| catalog.generation == self.generation)
            .and_then(|catalog| catalog.chats.as_deref())
            .or_else(|| {
                folders
                    .filter(|snapshot| {
                        snapshot.owner == self.owner
                            && snapshot.generation == self.generation
                            && snapshot.folders.is_some()
                    })
                    .map(|snapshot| snapshot.chats.as_slice())
            })
    }
}

/// Сводка для переключателя аккаунтов без учётных данных.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccountSnapshot {
    pub session: AccountSession, // Вход, которому принадлежит сводка.
    pub status: SessionStatus,   // Состояние восстановления или работы.
    pub unread: Option<u32>,     // Число «Все чаты»; None, если оно ещё не получено.
}

impl AccountSnapshot {
    /// Берёт только сведения, которые нужны оболочке для отображения аккаунта.
    pub fn from_session(snapshot: &SessionSnapshot, unread: Option<u32>) -> Self {
        Self {
            session: AccountSession {
                owner: snapshot.context.clone(),
                generation: snapshot.generation,
            },
            status: snapshot.status,
            unread,
        }
    }
}

/// Указывает начальную позицию и режим истории одного разговора.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatEntry {
    Latest,       // Открыть хвост истории.
    Around(Uuid), // Показать историю вокруг сообщения с этим UUID.
    Archived,     // Открыть хвост истории из архива без редактора.
}

/// Общий маршрут для перехода из списка или подтверждённого уведомления.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatRoute {
    pub owner: OwnerContext, // Аккаунт и проект, которому принадлежит разговор.
    pub scope: HistoryScope, // UUID разговора и, для темы канала, UUID темы.
    pub entry: ChatEntry,    // Начальная позиция в истории.
}

/// Цель перехода к последним сообщениям без указания конкретного сообщения.
#[derive(Clone, Copy)]
pub(super) enum ConversationReference {
    Stream(Uuid), // Канал или личная переписка.
    Topic {
        topic_id: Uuid,          // Точный UUID темы.
        stream_id: Option<Uuid>, // Ожидаемый родитель для ссылок с двумя UUID.
    },
    Contact(Uuid), // Существующая личная переписка с указанным пользователем.
}

/// Возвращает общую тему, если она есть в доступном каталоге разговора.
pub(super) fn default_topic(chat: &Chat) -> Option<Uuid> {
    chat.metadata
        .default_topic_id
        .filter(|id| chat.topics.iter().any(|topic| topic.id == *id))
}

/// Номер намерения пользователя, которым владеет оболочка.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NavigationRevision(pub u64); // Увеличивается при новом переходе и отмене ожидания.

/// Раздел «Моя активность», который открывается в рабочей области мессенджера.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActivityPage {
    Inbox,     // Разговоры и темы с активными непрочитанными.
    Favorites, // Личный чат с собой.
    Marked,    // Отмеченные сообщения.
    Mentions,  // Упомянувшие текущего пользователя сообщения.
    Drafts,    // Черновики владельца.
}

/// Текущий показ активности. Отложенное действие хранит этот адрес целиком.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityAddress {
    pub page: ActivityPage,           // Раздел, из которого началось действие.
    pub session: AccountSession,      // Вход, которому принадлежат данные страницы.
    pub revision: NavigationRevision, // Номер показа для проверки позднего результата.
}

/// Адрес открытого экрана, который фоновая команда сохраняет при запуске.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationAddress {
    pub route: ChatRoute,              // Владелец, разговор и начальная позиция.
    pub generation: SessionGeneration, // Номер входа на момент перехода.
    pub revision: NavigationRevision,  // Номер открытия, даже если маршрут уже открывался раньше.
}

/// Передаёт результат операции экрану, запустившему фоновую работу.
pub struct ConversationReply<T> {
    pub address: ConversationAddress, // Адрес в начале операции, не вычисляется заново при ответе.
    pub value: T,                     // Ответ операции; может содержать данные разговора.
}

impl<T> ConversationReply<T> {
    /// Отдаёт результат только открытому теперь экрану с тем же полным адресом.
    pub fn for_current(self, current: Option<&ConversationAddress>) -> Option<T> {
        (current == Some(&self.address)).then_some(self.value)
    }
}

/// Действие каталога, для которого требуется решение оболочки или core.
#[derive(Clone)]
pub enum SidebarAction {
    OpenChat {
        route: ChatRoute,              // Маршрут, выбранный мышью или клавиатурой.
        generation: SessionGeneration, // Поколение входа, чей каталог показан в момент выбора.
    },
    OpenCreatedTopic {
        session: AccountSession, // Вход, который получил подтверждение создания.
        stream_id: Uuid,          // UUID разговора, из меню которого создали тему.
        topic: Uuid,              // UUID новой темы, подтверждённый Core.
    },
    OpenActivity {
        page: ActivityPage,      // Раздел, выбранный в блоке «Моя активность».
        session: AccountSession, // Вход, чей блок был виден в момент выбора.
    },
    RetryCatalog(AccountSession), // Повторить загрузку того входа, чей каталог показан.
}

/// Намерение, которое получает оболочка от окна и системных адаптеров.
pub enum ShellAction {
    SelectAccount(AccountSession), // Сделать вход активным в этом окне.
    Sidebar(SidebarAction),        // Открыть строку каталога или повторить запрос.
    NotificationClicked(NotificationClick), // Проверить цель полученного баннера.
}

/// Запрос проверки цели до того, как оболочка откроет маршрут.
#[derive(Clone, Debug)]
pub struct NotificationRequest {
    pub revision: NavigationRevision, // Номер намерения на момент получения клика.
    pub click: NotificationClick,     // Цель с владельцем и поколением исходной сессии.
}

/// Результат проверки core для клика из системного центра уведомлений.
pub struct NotificationReply {
    pub request: NotificationRequest, // Прежний запрос для сверки намерения и поколения.
    pub result: Result<ChatRoute, NotificationRouteError>, // Подтверждённая цель либо причина отказа.
}

/// Причина, которую можно показать, сохранив текущий разговор.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotificationRouteError {
    InactiveSession,   // Исходный вход более не доступен.
    TargetUnavailable, // Сообщение исчезло или идентификатор цели непригоден.
    LookupFailed,      // Локальная проверка цели не завершилась.
}

impl From<NotificationTargetError> for NotificationRouteError {
    /// Сводит ошибку core к пользовательскому состоянию без деталей из сетевой среды.
    fn from(error: NotificationTargetError) -> Self {
        match error {
            NotificationTargetError::Auth(_) => Self::InactiveSession,
            NotificationTargetError::UnknownTarget => Self::TargetUnavailable,
            NotificationTargetError::Storage(_) => Self::LookupFailed,
        }
    }
}

/// Действие над папками для одного входа, переданное фоновому исполнителю.
#[derive(Clone, Eq, PartialEq)]
pub enum FolderCommand {
    Retry, // Запросить новый полный снимок.
    Create {
        id: Uuid,           // UUID, сохраняемый при повторе той же команды.
        title: String,      // Имя пользовательской папки.
        color: Option<u32>, // Цвет папки. None означает отсутствие цвета.
    },
    Update {
        id: Uuid,                   // UUID изменяемой пользовательской папки.
        title: Option<String>,      // Новое имя. None сохраняет прежнее.
        color: Option<Option<u32>>, // Новый цвет. Some(None) очищает его, None сохраняет.
    },
    Delete(Uuid), // Удалить папку с этим UUID.
    Add {
        folder: Uuid, // UUID пользовательской папки.
        chat: Uuid,   // UUID добавляемого разговора.
    },
    Remove {
        folder: Uuid, // UUID пользовательской папки.
        item: Uuid,   // UUID связи разговора с этой папкой.
    },
    Pin {
        folder: Uuid, // UUID папки, где действует закрепление.
        item: Uuid,   // UUID связи разговора с этой папкой.
        pinned: bool, // Истина закрепляет строку, ложь открепляет.
    },
}

/// Причина отказа, которую можно показать без вывода данных ответа или профиля.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FolderCommandError {
    Unknown,           // Сервер мог применить действие. До повтора нужна сверка.
    Rejected,          // Введённые данные или действие отклонены.
    Unavailable,       // Вход или данные для действия недоступны.
    Refresh,           // Действие выполнено, а запрос на локальную сверку не сохранился.
    WorkerUnavailable, // Команда не запущена или настройки не сохранились.
}

/// Локальный вид панели папок для аккаунта и проекта.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FolderRailPreferences {
    pub vertical: bool, // Вертикальная панель слева. Ложь размещает её горизонтально.
    pub show_system: bool, // Показывать «Личные» и «Каналы» вместе с «Все чаты».
}

impl Default for FolderRailPreferences {
    /// Открывает вертикальную панель и показывает системные папки.
    fn default() -> Self {
        Self {
            vertical: true,
            show_system: true,
        }
    }
}

/// Итог штатного завершения фонового исполнителя.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeExit {
    Clean,  // Фоновые задачи и начатые записи завершены.
    Failed, // Фоновый цикл завершился с ошибкой.
}

/// Событие рабочего потока, которое оболочка применяет в потоке окна.
pub enum RuntimeEvent {
    Accounts(Vec<AccountSnapshot>), // Полный снимок доступных сессий и их счётчиков.
    Catalog(ChatLists),             // Полный каталог; записи сверяются по владельцу и поколению.
    Folders(HashMap<OwnerContext, Arc<FolderSnapshot>>), // Папки доступных входов с поколением в каждом снимке.
    FolderPreferences(HashMap<OwnerContext, FolderRailPreferences>), // Сохранённый вид панели доступных входов.
    Sync(SyncUpdate),     // Изменение из core без тела сообщения и секретов.
    SyncLagged,           // Нужно перечитать снимки, включая открытую историю.
    Stopped(RuntimeExit), // Фоновый исполнитель закончил работу.
}

#[cfg(test)]
mod tests {
    use super::*;
    use workspace_core::context::AccountId;

    /// Готовит один и тот же маршрут с выбранной ревизией и поколением.
    fn address(revision: u64, generation: u64) -> ConversationAddress {
        ConversationAddress {
            route: ChatRoute {
                owner: OwnerContext {
                    account_id: AccountId("account-a".into()),
                    server: "https://workspace.test".into(),
                    organization_id: "org".into(),
                    project_id: "project".into(),
                    user_id: "user".into(),
                },
                scope: HistoryScope {
                    stream_id: Uuid::from_u128(1),
                    topic_id: None,
                },
                entry: ChatEntry::Latest,
            },
            generation: SessionGeneration(generation),
            revision: NavigationRevision(revision),
        }
    }

    /// Проверяет доставку запоздавшего ответа после возврата, повторного входа и смены темы.
    #[test]
    fn routes_replies_only_to_the_current_opening() {
        let first = address(1, 1);
        let reopened = address(3, 1);
        let signed_in_again = address(1, 2);
        let mut another_topic = first.clone();
        another_topic.route.scope.topic_id = Some(Uuid::from_u128(2));
        let mut another_owner = first.clone();
        another_owner.route.owner.project_id = "different-project".into();

        for current in [&reopened, &signed_in_again, &another_topic, &another_owner] {
            let reply = ConversationReply {
                address: first.clone(),
                value: "страница",
            };
            assert!(reply.for_current(Some(current)).is_none());
        }

        let reply = ConversationReply {
            address: first.clone(),
            value: "страница",
        };
        assert_eq!(reply.for_current(Some(&first)), Some("страница"));
    }
}
