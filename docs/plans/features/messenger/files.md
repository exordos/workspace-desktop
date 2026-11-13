# Файлы и медиа

[Мессенджер](index.md)

Модуль ведёт вложения от выбора в [редакторе](composer.md#msg-com-010) до отображения в [истории](history.md) и действий с локальной копией.

## Подготовка и загрузка вложений

| ID | Функция | Сценарий и границы | Основание | Арх. | Проект | Код |
| --- | --- | --- | --- | :---: | :---: | :---: |
| <a id="msg-file-001"></a>MSG-FILE-001 | Добавление вложения | Выбрать файл, перетащить его в редактор или вставить из буфера. Поддержать вложение и изображение внутри текста. | Референс · [загрузка вложений в редакторе](../../../../../workspace_ui/packages/web/src/features/workspace-composer-attachments/workspace-composer-attachments.model.ts) | □ | □ | □ |
| <a id="msg-file-002"></a>MSG-FILE-002 | Проверка вложения | Проверять размер и допустимость файла, показывать причину отказа рядом с вложением. | Референс · [загрузка вложений в редакторе](../../../../../workspace_ui/packages/web/src/features/workspace-composer-attachments/workspace-composer-attachments.model.ts) | □ | □ | □ |
| <a id="msg-file-003"></a>MSG-FILE-003 | Прогресс загрузки | Показать ход загрузки, позволить отменить, повторить или убрать вложение. | Референс · [загрузка вложений в редакторе](../../../../../workspace_ui/packages/web/src/features/workspace-composer-attachments/workspace-composer-attachments.model.ts) | □ | □ | □ |
| <a id="msg-file-004"></a>MSG-FILE-004 | Вложения в отправке | Использовать серверную ссылку только для успешно загруженного файла, связать её с сообщением в нужном разговоре. | Референс · [API файлов](../../../../../workspace_ui/packages/web/src/shared/api/messenger-files.api.ts) | □ | □ | □ |
| <a id="msg-file-005"></a>MSG-FILE-005 | Редактирование вложений | Сохранить существующие вложения при правке текста, добавить или убрать отдельный файл. | Референс · [вложения при редактировании](../../../../../workspace_ui/packages/web/src/features/workspace-composer-attachments/workspace-composer-edit-attachments.lib.ts) | □ | □ | □ |

## Отображение и просмотр

| ID | Функция | Сценарий и границы | Основание | Арх. | Проект | Код |
| --- | --- | --- | --- | :---: | :---: | :---: |
| <a id="msg-file-006"></a>MSG-FILE-006 | Карточка и миниатюра | Показать доступные метаданные файла, миниатюру и состояние ошибки. Загрузить превью с авторизацией исходного аккаунта. | Референс · [загрузка превью файла](../../../../../workspace_ui/packages/web/src/entities/messenger/messenger-workspace-message-file-preview.hook.ts) | □ | □ | □ |
| <a id="msg-file-007"></a>MSG-FILE-007 | Просмотр медиа | Открыть изображение в просмотрщике, переключать медиа, пользоваться миниатюрами и масштабированием. | Референс · [просмотр медиа](../../../../../workspace_ui/packages/web/src/features/media-viewer/media-viewer-overlay.ui.tsx) | □ | □ | □ |
| <a id="msg-file-008"></a>MSG-FILE-008 | Скачивание файла | Запустить авторизованное скачивание, выбрать место сохранения и показать прогресс или отказ. | Референс · [десктопные загрузки](../../../../../workspace_ui/packages/electron/src/workspace-downloads.ts) | □ | □ | □ |
| <a id="msg-file-009"></a>MSG-FILE-009 | Действия со скачиванием | Отменить загрузку, открыть сохранённый файл, показать в папке или убрать запись из списка. | Референс · [десктопные загрузки](../../../../../workspace_ui/packages/electron/src/workspace-downloads.ts) | □ | □ | □ |
| <a id="msg-file-010"></a>MSG-FILE-010 | Доступ к защищённому вложению | Открывать вложение при доступе к ссылке и серверному файлу, показать причину отказа. [Область защиты](encryption.md#msg-e2ee-007). | Референс · [ограничения вложений в E2EE](../../../../../workspace_ui/docs/E2EE.md#capabilities) | □ | □ | □ |
| <a id="msg-file-011"></a>MSG-FILE-011 | Очистка после отзыва доступа | Удалить затронутые данные из локального файлового кэша. | Референс · [файловый кэш и загрузка](../../../../../workspace_ui/packages/web/src/shared/lib/workspace-file-loader.lib.ts) · [отзыв доступа и очистка](../../workspace-desktop-architecture.md#фоновая-обработка-и-замеры) | [✓](../../workspace-desktop-architecture.md#фоновая-обработка-и-замеры) | □ | □ |
