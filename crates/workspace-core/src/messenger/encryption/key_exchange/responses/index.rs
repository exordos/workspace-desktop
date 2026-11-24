//! Проверяет найденные в истории ответы перед скрытием действий и повторной отправкой.

use crate::{messenger::encryption::grant::decode_grant_envelope, storage::KeyRequestHint};

use super::{
    EncryptionActivation, KeyExchange, KeyExchangeError, KeyRequestOutcome, KeyRequestTarget,
    KeyServiceMessage, KeyServicePayload, response_details,
};

/// Что подтверждено о сохранённых возможных ответах на точный запрос.
#[derive(PartialEq, Eq)]
pub(super) enum ReplyResolution {
    Pending,                     // Ни один известный адрес не содержит разрешённого ответа.
    Resolved(KeyRequestOutcome), // Сервер вернул подходящий ответ с проверенным правом автора.
    Unverified,                  // Ответ по одному из адресов временно не удалось прочитать.
}

impl KeyExchange {
    /// Использует локальный индекс только для поиска адресов. Ответ и права читает с сервера.
    pub(super) async fn confirm_reply_resolution(
        &self,
        activation: &EncryptionActivation,
        token: &str,
        target: KeyRequestTarget,
        request: &KeyServiceMessage,
    ) -> Result<ReplyResolution, KeyExchangeError> {
        let KeyServicePayload::Request {
            device_id,
            device_name,
            ..
        } = &request.payload
        else {
            return Err(KeyExchangeError::InvalidResponse);
        };
        let query = KeyRequestHint {
            stream_id: target.stream_id,
            topic_id: target.topic_id,
            key_id: target.key_id,
            request_id: request.id,
            device_id: device_id.clone(),
            device_name: device_name.clone(),
        };
        let session = activation.session();
        let mut after = None;
        let mut resolution = ReplyResolution::Pending;
        loop {
            self.require_current(activation)?;
            let candidates = self.sync.auth().with_active_generation(
                &session.owner,
                session.generation,
                || {
                    self.sync
                        .storage()
                        .key_reply_candidates(&session.owner, &query, after)
                },
            )??;
            let Some(last) = candidates.last().copied() else {
                break;
            };
            after = Some(last);
            for id in candidates {
                let response = self
                    .messenger
                    .rest
                    .lookup_key_reply_candidate(&session.owner, token, target, id)
                    .await;
                self.require_current(activation)?;
                let response = match response {
                    Ok(Some(response)) => response,
                    Ok(None) => continue,
                    Err(_) => {
                        resolution = ReplyResolution::Unverified;
                        continue;
                    }
                };
                if !response.matches_request(request) {
                    continue;
                }
                let Some((_, outcome)) = response_details(&response.payload) else {
                    continue;
                };
                if let KeyServicePayload::Grant { content, .. } = &response.payload
                    && decode_grant_envelope(content).is_none()
                {
                    continue;
                }
                // Наблюдатель проверяет формат и автора. HPKE может открыть только получатель.
                let policy = self.reply_policy(activation, token, target).await?;
                if policy.permits_reply(response.author_id, request.author_id, outcome, false) {
                    return Ok(ReplyResolution::Resolved(outcome));
                }
            }
        }
        Ok(resolution)
    }
}
