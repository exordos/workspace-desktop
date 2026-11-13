# Разговоры, каналы и темы

[Мессенджер](index.md)

Здесь собраны операции с разговорами и участниками. [Список и папки](navigation.md), [история разговора](history.md) и [редактор](composer.md) используют эти данные. Сценарий создания защищённого разговора описан в [шифровании](encryption.md#msg-e2ee-001).

## Разговоры

| ID | Функция | Сценарий и границы | Основание | Арх. | Проект | Код |
| --- | --- | --- | --- | :---: | :---: | :---: |
| <a id="msg-chat-001"></a>MSG-CHAT-001 | Личный чат | Найти собеседника, открыть существующий разговор или создать private stream с одним собеседником. | Референс · [создание разговоров](../../../../../workspace_ui/packages/web/src/entities/messenger/messenger-create-chat-actions.lib.ts) | □ | □ | □ |
| <a id="msg-chat-002"></a>MSG-CHAT-002 | Чат с собой | Найти или создать разговор с собой для заметок и пересланных сообщений. [Пункт «Избранное»](activity.md#msg-act-007) открывает его из навигации. | Референс · [чат с собой](../../../../../workspace_ui/packages/web/src/features/workspace-self-chat/workspace-self-chat.hook.ts) | □ | □ | □ |
| <a id="msg-chat-003"></a>MSG-CHAT-003 | Групповые разговоры | Сверить, нужен ли отдельный тип многопользовательского DM. В основной ветке UI различает личный поток и канал. | Уточнить · [определение типов чатов](../../../../../workspace_ui/packages/web/src/entities/messenger/messenger-conversation-ui-kind.lib.ts) | □ | □ | □ |
| <a id="msg-chat-004"></a>MSG-CHAT-004 | Создание канала | Указать название, описание и начальных участников. Обработать отказ при создании и ошибку добавления участников. | Референс · [создание разговоров](../../../../../workspace_ui/packages/web/src/entities/messenger/messenger-create-chat-actions.lib.ts) | □ | □ | □ |
| <a id="msg-chat-005"></a>MSG-CHAT-005 | Канал по приглашению | Выбрать предусмотренный в диалоге режим закрытого канала и показать ограничения доступа. | Референс · [диалог создания](../../../../../workspace_ui/packages/web/src/features/create-chat/create-chat-dialog.hook.ts) | □ | □ | □ |
| <a id="msg-chat-007"></a>MSG-CHAT-007 | Просмотр сведений | Открыть карточку канала из меню или заголовка, показать сведения о разговоре и доступные действия. | Референс · [информация о канале и участники](../../../../../workspace_ui/packages/web/src/widgets/right-panel/right-panel-workspace-info.ui.tsx) | □ | □ | □ |
| <a id="msg-chat-008"></a>MSG-CHAT-008 | Переименование канала | Изменить название с проверкой прав и синхронизировать его в списке, заголовке и карточке. | Референс · [действия с каналами](../../../../../workspace_ui/packages/web/src/entities/messenger/messenger-sidebar-actions.lib.ts) | □ | □ | □ |
| <a id="msg-chat-009"></a>MSG-CHAT-009 | Архивирование разговора | Подтвердить архивацию личного чата или канала. Убрать его из активного каталога, сохранив историю. | Референс · [действия строки чата](../../../../../workspace_ui/packages/web/src/widgets/sidebar/sidebar-workspace-context-menu.ui.tsx) | □ | □ | □ |
| <a id="msg-chat-010"></a>MSG-CHAT-010 | Просмотр архива | Найти архивный разговор, открыть его историю и обработать ошибку загрузки списка. | Референс · [диалог создания](../../../../../workspace_ui/packages/web/src/features/create-chat/create-chat-dialog.hook.ts) | □ | □ | □ |
| <a id="msg-chat-011"></a>MSG-CHAT-011 | Восстановление из архива | Вернуть разговор в активный список. Показать состояние операции и позволить повтор после ошибки. | Референс · [диалог создания](../../../../../workspace_ui/packages/web/src/features/create-chat/create-chat-dialog.hook.ts) | □ | □ | □ |
| <a id="msg-chat-012"></a>MSG-CHAT-012 | Обзор и подписка на каналы | Уточнить поиск по общему каталогу, фильтры подписки и присоединение. Основной диалог Workspace скрывает этот сценарий. | Уточнить · [каталог каналов отключён в Workspace](../../../../../workspace_ui/packages/web/src/features/create-chat/create-chat-dialog.hook.ts) | □ | □ | □ |
| <a id="msg-chat-013"></a>MSG-CHAT-013 | Удаление канала | Сверить права, подтверждение и доступный пользовательский маршрут. В клиенте есть вызов API. | Уточнить · [API удаления канала](../../../../../workspace_ui/packages/web/src/shared/api/messenger-streams.api.ts) | □ | □ | □ |

## Участники и права

| ID | Функция | Сценарий и границы | Основание | Арх. | Проект | Код |
| --- | --- | --- | --- | :---: | :---: | :---: |
| <a id="msg-chat-014"></a>MSG-CHAT-014 | Список участников | Показать состав, роли, состояние присутствия и доступные участнику действия. | Референс · [информация о канале и участники](../../../../../workspace_ui/packages/web/src/widgets/right-panel/right-panel-workspace-info.ui.tsx) | □ | □ | □ |
| <a id="msg-chat-015"></a>MSG-CHAT-015 | Добавление участников | Найти и выбрать нескольких пользователей, исключить уже добавленных, показать частичный сбой или ошибку. | Референс · [действия с участниками](../../../../../workspace_ui/packages/web/src/entities/messenger/messenger-stream-member-actions.lib.ts) | □ | □ | □ |
| <a id="msg-chat-016"></a>MSG-CHAT-016 | Удаление участника | Проверить право на удаление. Обновить состав после подтверждения сервера. | Референс · [действия с участниками](../../../../../workspace_ui/packages/web/src/entities/messenger/messenger-stream-member-actions.lib.ts) | □ | □ | □ |
| <a id="msg-chat-017"></a>MSG-CHAT-017 | Выход из канала | Дать участнику удалить собственную привязку и согласовать потерю доступа с кэшем и навигацией. | Референс · [информация о канале и участники](../../../../../workspace_ui/packages/web/src/widgets/right-panel/right-panel-workspace-info.ui.tsx) | □ | □ | □ |
| <a id="msg-chat-018"></a>MSG-CHAT-018 | Изменение роли и приглашение по ссылке | Сверить отдельные пользовательские действия, права и серверный контракт. | Уточнить · [API привязок участников](../../../../../workspace_ui/packages/web/src/shared/api/messenger-streams.api.ts) | □ | □ | □ |

## Темы

| ID | Функция | Сценарий и границы | Основание | Арх. | Проект | Код |
| --- | --- | --- | --- | :---: | :---: | :---: |
| <a id="msg-chat-019"></a>MSG-CHAT-019 | Тема по умолчанию | Найти созданную сервером тему по умолчанию. Обработать её отсутствие и отказ в отправке без подходящей темы. | Референс · [создание разговоров](../../../../../workspace_ui/packages/web/src/entities/messenger/messenger-create-chat-actions.lib.ts) | □ | □ | □ |
| <a id="msg-chat-020"></a>MSG-CHAT-020 | Просмотр тем | Загрузить темы выбранного канала, включая завершённые, и открывать их по UUID. | Референс · [API тем](../../../../../workspace_ui/packages/web/src/shared/api/messenger-topics.api.ts) | □ | □ | □ |
| <a id="msg-chat-021"></a>MSG-CHAT-021 | Создание темы | Выбрать канал, ввести название, создать тему и открыть её после ответа. | Референс · [диалог создания](../../../../../workspace_ui/packages/web/src/features/create-chat/create-chat-dialog.hook.ts) | □ | □ | □ |
| <a id="msg-chat-022"></a>MSG-CHAT-022 | Переименование темы | Переименовать тему и сохранить её идентичность в открытом разговоре и списках. | Референс · [действия темы](../../../../../workspace_ui/packages/web/src/features/workspace-topic-actions/workspace-topic-context-menu.ui.tsx) | □ | □ | □ |
| <a id="msg-chat-023"></a>MSG-CHAT-023 | Завершение и повторное открытие | Отметить тему завершённой или снять отметку. Показать условие для продолжения переписки в завершённой теме. | Референс · [снятие отметки завершения](../../../../../workspace_ui/packages/web/src/pages/chat/chat-page-stream-topic-prompt.ui.tsx) | □ | □ | □ |
| <a id="msg-chat-024"></a>MSG-CHAT-024 | Удаление темы | Сверить пункт меню, подтверждение и права. API удаления присутствует, а обработка события удаляет данные темы. | Уточнить · [API удаления темы](../../../../../workspace_ui/packages/web/src/shared/api/messenger-topics.api.ts) | □ | □ | □ |
| <a id="msg-chat-025"></a>MSG-CHAT-025 | Перенос темы в другой канал | Уточнить контракт переноса, сохранение истории и переход по старой ссылке. В проверенной ветке UI вызов переноса заглушён. | Уточнить · [заглушка переноса в UI](../../../../../workspace_ui/packages/web/src/features/move-topic-to-stream/move-topic-to-stream.api.ts) | □ | □ | □ |
| <a id="msg-chat-028"></a>MSG-CHAT-028 | Смена темы по умолчанию | Уточнить пользовательский маршрут и права. Серверный контракт задаёт действие `set_default`. | Уточнить · [серверный контракт тем](../../../../../workspace_backend/docs/en/workspace_api.md#stream-topics) | □ | □ | □ |

## Дополнительные настройки

| ID | Функция | Сценарий и границы | Основание | Арх. | Проект | Код |
| --- | --- | --- | --- | :---: | :---: | :---: |
| <a id="msg-chat-026"></a>MSG-CHAT-026 | Объявление о канале | При создании передать выбранный пользователем флаг объявления о канале. | Референс · [настройка объявления канала](../../../../../workspace_ui/packages/web/src/features/create-chat/create-chat-dialog.hook.ts) | □ | □ | □ |
| <a id="msg-chat-027"></a>MSG-CHAT-027 | Канал только для объявлений | Уточнить, нужны ли ограничения на публикацию и каким ролям разрешена отправка. Настройка доступна только как заготовка интерфейса. | Уточнить · [отключённая политика канала](../../../../../workspace_ui/packages/web/src/features/create-chat/create-chat-dialog.ui.tsx) | □ | □ | □ |
