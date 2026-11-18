# Пользователи и присутствие

[Общие модули](index.md) · [Навигация мессенджера](../messenger/navigation.md) · [Синхронизация](sync.md)

| ID | Функция | Сценарий и границы | Основание | Арх. | Проект | Код |
| --- | --- | --- | --- | --- | --- | --- |
| <a id="dir-001"></a>DIR-001 | Каталог пользователей | Загрузить пользователей текущего контекста, обновлять их по серверным событиям и предоставлять данные экранам и подборщикам участников. | Референс · [синхронизация пользователей](../../../../../workspace_ui/packages/web/src/entities/user/user-sync.lib.ts) | [✓](../../workspace-desktop-architecture.md#путь-события) | □ | □ |
| <a id="dir-002"></a>DIR-002 | Поиск людей | Найти пользователя из общего поиска, открыть профиль или личный чат. Для поиска чатов и сообщений см. [навигацию](../messenger/navigation.md) и [активность](../messenger/activity.md). | Референс · [общий поиск](../../../../../workspace_ui/packages/web/src/widgets/search-modal/) | □ | □ | □ |
| <a id="dir-003"></a>DIR-003 | Карточка профиля | Показать имя, аватар, присутствие и доступные поля. Открыть личный чат, скопировать доступные идентификаторы или ссылку профиля. | Референс · [карточка и действия](../../../../../workspace_ui/packages/web/src/widgets/right-panel/right-panel-user-profile.ui.tsx) | □ | □ | □ |
| <a id="dir-004"></a>DIR-004 | Свой профиль | Просмотреть и редактировать поля, которые разрешает текущий API профиля. Показать ошибки валидации, прав и сохранения. | Референс · [профиль](../../../../../workspace_ui/packages/web/src/features/user-profile/) · [IAM API](../../../../../workspace_ui/packages/web/src/shared/api/workspace-iam-profile.api.ts) | □ | □ | □ |
| <a id="dir-005"></a>DIR-005 | Аватар | Показать аватар и его предпросмотр, выбрать новый или удалить текущий. Учесть ограничения файла и доступность операции. | Референс · [аватар](../../../../../workspace_ui/packages/web/src/features/workspace-avatar/) | □ | □ | □ |
| <a id="dir-006"></a>DIR-006 | Статус присутствия | Показывать актуальное присутствие собеседников и передавать состояние пользователя с учётом активности клиента. | Референс · [отправка присутствия](../../../../../workspace_ui/packages/web/src/entities/user/user-workspace-presence-reporter.lib.ts) · [присутствие и статус](../../../../../workspace_ui/docs/PRESENCE_AND_STATUS.md) | [✓](../../workspace-desktop-architecture.md#модули-и-ответственность) | □ | □ |
| <a id="dir-007"></a>DIR-007 | Пользовательский статус | Установить или очистить текст и эмодзи, переключить режим «Отошёл». | Референс · [диалог статуса](../../../../../workspace_ui/packages/web/src/widgets/right-panel/right-panel-user-menu-status-dialog.ui.tsx) · [действия](../../../../../workspace_ui/packages/web/src/entities/user/user-workspace-status-actions.lib.ts) | □ | □ | □ |

[План Core для DIR-005](../../modules/directory.md) описывает кэш аватаров и синхронизацию. Подключение экранов и изменение собственного фото относятся к следующим этапам сценария.
