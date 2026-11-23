//! Открывает правку смешанного Markdown и проверяет исходную строку и все ключи до PUT.

use std::{
    collections::HashSet,
    future::{Future, poll_fn},
    sync::{Arc, Mutex, Weak},
    task::Poll,
};

use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    auth::Service,
    context::OwnerContext,
    messenger::{MessengerSource, message_actions::MessageTarget},
    storage::{HistoryMessage, HistoryScope},
    sync::{FragmentOriginsPermit, MAX_MESSAGE_CHARACTERS, Sync, SyncError},
};

use super::{
    EncryptionActivation, FragmentError, FragmentMessageError, RestoredFragmentDraft, Selection,
    editable_fragment_links, has_protected_draft, seal_fragments,
};

/// Снимок правки, связанный с точным сообщением, активацией и ключами всех ссылок.
pub struct FragmentEdit {
    activation: EncryptionActivation, // Аккаунт и поколение входа, которые открыли редактор.
    selection: Weak<Mutex<Option<Selection>>>, // Актуальная активация в том же сервисе.
    target: MessageTarget,            // Подтверждённое нативное размещение.
    source: HistoryMessage,           // Канонический исходник, в котором фрагменты зашифрованы.
    restored: Option<RestoredFragmentDraft>, // Открытые фрагменты только в памяти редактора.
    origins: FragmentOriginsPermit,   // Версии доступа ко всем чатам, участвующим в правке.
}

impl FragmentEdit {
    /// Читает Markdown редактора, пока доступны исходные ключи и не менялся вход.
    pub fn with_markdown<T>(&self, read: impl FnOnce(&str) -> T) -> Option<T> {
        let selection = self.selection.upgrade()?;
        let current = selection.lock().unwrap();
        if !self.is_current(current.as_ref()) {
            return None;
        }
        let markdown = self
            .restored
            .as_ref()
            .map(|value| value.markdown.as_str())
            .or_else(|| self.source.body.markdown())?;
        self.origins.with_current(|| read(markdown))
    }

    /// Сверяет сам выбор аккаунта и исходные версии, не удерживая ключи при правке.
    fn is_current(&self, selection: Option<&Selection>) -> bool {
        selection.is_some_and(|value| value.activation == self.activation)
            && self
                .restored
                .as_ref()
                .is_none_or(|value| value.is_current(selection))
    }
}

impl MessengerSource {
    /// Открывает редактор своего нативного сообщения из точного снимка истории.
    pub async fn open_fragment_edit(
        &self,
        sync: &Sync,
        activation: &EncryptionActivation,
        target: MessageTarget,
        expected: &HistoryMessage,
    ) -> Result<FragmentEdit, FragmentMessageError> {
        let profile = self.encryption_profile(activation)?;
        if !target.is_valid()
            || (expected.stream_id, expected.topic_id, expected.id)
                != (target.scope.stream_id, target.topic_id, target.message_id)
        {
            return Err(FragmentMessageError::StaleEdit);
        }
        let markdown = expected
            .body
            .markdown()
            .ok_or(FragmentMessageError::StaleEdit)?;
        let auth = sync.auth();
        let session = activation.session();
        let streams = editable_fragment_links(markdown)?
            .into_iter()
            .map(|link| link.reference.stream_id())
            .chain(std::iter::once(expected.stream_id))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let origins =
            sync.prepare_fragment_origins(&session.owner, session.generation, &streams)?;
        let token = auth
            .access_token_for_generation(&session.owner, Service::Messenger, session.generation)
            .await?;
        self.rest
            .fragment_send_topic(
                &session.owner,
                &token,
                HistoryScope {
                    stream_id: target.scope.stream_id,
                    topic_id: Some(target.topic_id),
                },
            )
            .await?;
        self.check_fragment_edit_origins(&session.owner, &token, target.scope.stream_id, &origins)
            .await?;
        let source = self
            .rest
            .markdown_edit_source(&session.owner, &token, target)
            .await?;
        if &source != expected {
            return Err(FragmentMessageError::StaleEdit);
        }
        auth.with_active_generation(&session.owner, session.generation, || ())?;
        let restored = profile.keys.restore_fragment_draft(activation, markdown)?;
        let edit = FragmentEdit {
            activation: activation.clone(),
            selection: Arc::downgrade(&profile.keys.selection),
            target,
            source,
            restored,
            origins,
        };
        edit.with_markdown(|_| ())
            .ok_or(FragmentMessageError::StaleEdit)?;
        Ok(edit)
    }

    /// Запечатывает новые диапазоны и отправляет один PUT, если исходник и ключи ещё действуют.
    pub async fn edit_fragment_message(
        &self,
        sync: &Sync,
        edit: &FragmentEdit,
        markdown: String,
    ) -> Result<HistoryMessage, FragmentMessageError> {
        let markdown = Zeroizing::new(markdown);
        if markdown.trim().is_empty() {
            return Err(FragmentError::InvalidFragment.into());
        }
        edit.with_markdown(|_| ())
            .ok_or(FragmentMessageError::StaleEdit)?;
        let keys = &self.encryption_profile(&edit.activation)?.keys;
        if !edit.selection.ptr_eq(&Arc::downgrade(&keys.selection)) {
            return Err(FragmentMessageError::StaleEdit);
        }
        let auth = sync.auth();
        let session = edit.activation.session();
        let token = auth
            .access_token_for_generation(&session.owner, Service::Messenger, session.generation)
            .await?;
        let scope = HistoryScope {
            stream_id: edit.target.scope.stream_id,
            topic_id: Some(edit.target.topic_id),
        };
        self.rest
            .fragment_send_topic(&session.owner, &token, scope)
            .await?;
        self.check_fragment_edit_origins(&session.owner, &token, scope.stream_id, &edit.origins)
            .await?;
        let current = self
            .rest
            .markdown_edit_source(&session.owner, &token, edit.target)
            .await?;
        if current != edit.source {
            return Err(FragmentMessageError::StaleEdit);
        }
        let (sealed, outgoing_key) = if has_protected_draft(&markdown) {
            let key = keys.fragment_sending_key(&edit.activation, scope.stream_id, false)?;
            let sealed = seal_fragments(&markdown, &key)?;
            keys.authorize_outgoing(&key, true, || ())?;
            (sealed, Some(key))
        } else {
            if markdown.chars().count() > MAX_MESSAGE_CHARACTERS {
                return Err(FragmentError::TooLong.into());
            }
            (markdown.to_string(), None)
        };

        let mut request = std::pin::pin!(self.rest.edit_markdown_once(
            &session.owner,
            &token,
            edit.target,
            &sealed
        ));
        let mut started = false;
        let result = poll_fn(|context| {
            if started {
                return request.as_mut().poll(context).map(Ok);
            }
            let admitted = auth.with_active_generation(&session.owner, session.generation, || {
                // Не держим системное чтение в сетевом запросе. Если операция уже заняла
                // хранилище, меняющуюся правку можно повторить до начала передачи.
                let _operation = keys
                    .store_operations
                    .try_lock()
                    .map_err(|_| FragmentMessageError::Busy)?;
                let selected = keys.selection.lock().unwrap();
                if !edit.is_current(selected.as_ref())
                    || outgoing_key
                        .as_ref()
                        .is_some_and(|key| !key.access.is_current(selected.as_ref()))
                    || !sync
                        .storage()
                        .matches_markdown_edit(&session.owner, &edit.source)
                        .map_err(SyncError::from)?
                {
                    return Err(FragmentMessageError::StaleEdit);
                }
                edit.origins
                    .with_current(|| {
                        started = true;
                        request.as_mut().poll(context)
                    })
                    .ok_or(FragmentMessageError::StaleEdit)
            });
            match admitted {
                Ok(Ok(poll)) => poll.map(Ok),
                Ok(Err(error)) => Poll::Ready(Err(error)),
                Err(error) => Poll::Ready(Err(error.into())),
            }
        })
        .await??;
        auth.with_active_generation(&session.owner, session.generation, || ())?;
        edit.with_markdown(|_| ())
            .ok_or(FragmentMessageError::StaleEdit)?;
        Ok(result)
    }

    /// Подтверждает каждый исходный чат ссылок один раз на попытку открыть или сохранить правку.
    async fn check_fragment_edit_origins(
        &self,
        owner: &OwnerContext,
        token: &str,
        verified_stream: Uuid,
        origins: &FragmentOriginsPermit,
    ) -> Result<(), FragmentMessageError> {
        for stream_id in origins
            .streams()
            .filter(|&stream| stream != verified_stream)
        {
            self.rest
                .fragment_source_topic(owner, token, stream_id)
                .await?;
        }
        origins
            .with_current(|| ())
            .ok_or(FragmentMessageError::StaleEdit)
    }
}
