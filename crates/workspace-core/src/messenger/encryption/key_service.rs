//! Описывает служебные сообщения E2EE и связь ответа с запросом устройства.
//! Данные ключа остаются строками до операции HPKE, чтобы читать повреждённые записи истории.

use std::collections::HashMap;

use uuid::Uuid;

use super::super::details::StreamRole;
use super::grant::GrantContext;
use super::key_exchange::KeyRequestOutcome;

pub use crate::storage::{KeyResponseTarget, KeyServicePayload};

impl KeyResponseTarget {
    /// Привязывает выдачу к чату, в котором сервер разместил запрос.
    pub fn grant_context(&self, stream_id: Uuid) -> GrantContext<'_> {
        GrantContext {
            stream_id,
            key_id: self.key_uuid,
            request_id: self.request_message_uuid,
            sender_device_id: &self.sender_device_id,
            recipient_device_id: &self.recipient_device_id,
        }
    }
}

/// Проверенное размещение служебного сообщения в истории конкретного владельца данных.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyServiceMessage {
    pub id: Uuid,                   // Публичный UUID сообщения, назначенный сервером.
    pub stream_id: Uuid,            // Стрим, в котором размещено сообщение.
    pub topic_id: Uuid,             // Тема запроса или ответа.
    pub author_id: Uuid,            // Автор по серверным метаданным для проверки доступа.
    pub payload: KeyServicePayload, // Тело запроса, объявления, выдачи или отказа.
}

impl KeyServiceMessage {
    /// Создаёт адрес ответа из опубликованного запроса устройства.
    pub fn reply_target(&self, sender_device_id: &str) -> Option<KeyResponseTarget> {
        let KeyServicePayload::Request {
            key_uuid,
            device_id,
            ..
        } = &self.payload
        else {
            return None;
        };
        if sender_device_id.is_empty() || device_id.is_empty() {
            return None;
        }
        Some(KeyResponseTarget {
            key_uuid: *key_uuid,
            request_message_uuid: self.id,
            sender_device_id: sender_device_id.to_owned(),
            recipient_device_id: device_id.clone(),
        })
    }

    /// Проверяет связь ответа с запросом по размещению, поколению и устройству.
    /// Права автора и актуальность выбранного входа проверяет вызывающая операция.
    pub fn matches_request(&self, request: &Self) -> bool {
        let target = match &self.payload {
            KeyServicePayload::Grant { target, .. } | KeyServicePayload::Reject(target) => target,
            _ => return false,
        };
        let KeyServicePayload::Request {
            key_uuid,
            device_id,
            ..
        } = &request.payload
        else {
            return false;
        };
        self.stream_id == request.stream_id
            && self.topic_id == request.topic_id
            && target.request_message_uuid == request.id
            && target.key_uuid == *key_uuid
            && target.recipient_device_id == *device_id
            && !target.sender_device_id.is_empty()
            && !device_id.is_empty()
    }
}

/// Авторитетный снимок состава и режима выдачи из API проекта.
pub(in crate::messenger) struct KeySharingPolicy {
    pub stream_owner: Option<Uuid>, // Канонический владелец, если сервер его указал.
    pub protected: bool,            // Полностью зашифрованный чат.
    pub holders: Option<bool>,      // Новая политика. `None` означает правила старого клиента.
    pub roles: HashMap<Uuid, StreamRole>, // Подтверждённые текущие привязки участников.
}

impl KeySharingPolicy {
    /// Учитывает точный локальный ключ при выдаче и при отказе обычного участника.
    pub(in crate::messenger) fn permits_outbound_reply(
        &self,
        issuer: Uuid,
        requester: Uuid,
        outcome: KeyRequestOutcome,
        has_key: bool,
    ) -> bool {
        if self.protected {
            return self.permits_reply(issuer, requester, outcome, true)
                && (outcome == KeyRequestOutcome::Rejected || has_key);
        }
        self.member(requester)
            && self.member(issuer)
            && (has_key || (outcome == KeyRequestOutcome::Rejected && self.manager(issuer)))
    }

    /// Проверяет права автора. При выдаче наличие ключа проверяется отдельно в хранилище.
    pub(in crate::messenger) fn permits_reply(
        &self,
        issuer: Uuid,
        requester: Uuid,
        outcome: KeyRequestOutcome,
        issuing: bool,
    ) -> bool {
        if self.protected {
            if outcome == KeyRequestOutcome::Rejected {
                return self.manager(issuer);
            }
            return match self.holders {
                Some(true) => self.member(issuer) && self.member(requester),
                Some(false) => self.stream_owner == Some(issuer),
                None => self.manager(issuer),
            };
        }
        self.member(requester)
            && if issuing {
                self.manager(issuer)
            } else {
                self.member(issuer)
            }
    }

    /// Определяет участника по действующей привязке или каноническому владению.
    fn member(&self, user: Uuid) -> bool {
        self.stream_owner == Some(user) || self.roles.contains_key(&user)
    }

    /// Разрешает управление владельцу или администратору канала.
    fn manager(&self, user: Uuid) -> bool {
        self.stream_owner == Some(user)
            || matches!(
                self.roles.get(&user),
                Some(StreamRole::Administrator | StreamRole::Owner)
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const KEY_ID: Uuid = uuid::uuid!("01ea4b50-fb07-4444-8111-5b954ee66cf1");
    const REQUEST_ID: Uuid = uuid::uuid!("ec056269-97a7-42c1-8c8d-32598e616702");

    /// Сверяет старую политику, выдачу только владельцем и выдачу участникам с ключом.
    #[test]
    fn resolves_legacy_owner_and_holder_sharing_policy() {
        use KeyRequestOutcome::{Granted, Rejected};
        let [owner, administrator, member, requester, outsider] = [(); 5].map(|()| Uuid::new_v4());
        let mut policy = KeySharingPolicy {
            protected: true,
            holders: None,
            stream_owner: Some(owner),
            roles: HashMap::from([
                (administrator, StreamRole::Administrator),
                (member, StreamRole::Member),
                (requester, StreamRole::Member),
            ]),
        };
        assert!(policy.permits_reply(administrator, requester, Granted, false));
        assert!(!policy.permits_reply(member, requester, Granted, true));
        policy.holders = Some(false);
        assert!(policy.permits_reply(owner, requester, Granted, false));
        assert!(!policy.permits_reply(administrator, requester, Granted, false));
        assert!(policy.permits_reply(administrator, requester, Rejected, true));
        policy.holders = Some(true);
        assert!(policy.permits_reply(member, requester, Granted, true));
        assert!(!policy.permits_reply(outsider, requester, Granted, false));
        assert!(!policy.permits_reply(member, outsider, Granted, true));
        assert!(!policy.permits_reply(member, requester, Rejected, false));
        policy.protected = false;
        assert!(policy.permits_reply(member, requester, Granted, false));
        assert!(!policy.permits_reply(member, requester, Granted, true));
        assert!(policy.permits_outbound_reply(member, requester, Granted, true));
        assert!(policy.permits_outbound_reply(member, requester, Rejected, true));
        assert!(!policy.permits_outbound_reply(member, requester, Rejected, false));
        assert!(!policy.permits_outbound_reply(administrator, requester, Granted, false));
        assert!(policy.permits_outbound_reply(administrator, requester, Rejected, false));
        assert!(!policy.permits_outbound_reply(outsider, requester, Granted, true));
        assert!(!policy.permits_outbound_reply(member, outsider, Rejected, true));
        policy.protected = true;
        policy.holders = Some(false);
        assert!(policy.permits_outbound_reply(owner, requester, Granted, true));
        assert!(!policy.permits_outbound_reply(administrator, requester, Granted, true));
        assert!(policy.permits_outbound_reply(administrator, requester, Rejected, false));
    }

    /// Сверяет wire-поля REST с публичной схемой текущих Workspace клиентов.
    #[test]
    fn round_trips_each_key_service_payload() {
        for wire in [
            json!({"kind":"key_request", "key_uuid":KEY_ID, "device_id":"device-A", "device_name":"ПК", "public_key":"unvalidated"}),
            json!({"kind":"key_grant", "key_uuid":KEY_ID, "request_message_uuid":REQUEST_ID, "sender_device_id":"device-B", "recipient_device_id":"device-A", "content":"unvalidated"}),
            json!({"kind":"key_reject", "key_uuid":KEY_ID, "request_message_uuid":REQUEST_ID, "sender_device_id":"device-B", "recipient_device_id":"device-A"}),
            json!({"kind":"key_announced", "key_uuid":KEY_ID}),
        ] {
            let payload: KeyServicePayload = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(payload.key_id(), KEY_ID);
            assert_eq!(serde_json::to_value(payload).unwrap(), wire);
        }
    }

    /// Ответ можно применить только к опубликованному запросу из исходной темы.
    #[test]
    fn rejects_response_for_another_request_topic_or_device() {
        let request = KeyServiceMessage {
            id: REQUEST_ID,
            stream_id: Uuid::new_v4(),
            topic_id: Uuid::new_v4(),
            author_id: Uuid::new_v4(),
            payload: KeyServicePayload::Request {
                key_uuid: KEY_ID,
                device_id: "device-A".to_owned(),
                device_name: "ПК".to_owned(),
                public_key: "unvalidated".to_owned(),
            },
        };
        let target = request.reply_target("device-B").unwrap();
        let mut response = KeyServiceMessage {
            id: Uuid::new_v4(),
            author_id: Uuid::new_v4(),
            payload: KeyServicePayload::Grant {
                target: target.clone(),
                content: "unvalidated".to_owned(),
            },
            ..request.clone()
        };
        assert!(response.matches_request(&request));
        response.payload = KeyServicePayload::Reject(target.clone());
        assert!(response.matches_request(&request));
        assert!(!response.matches_request(&KeyServiceMessage {
            id: Uuid::new_v4(),
            ..request.clone()
        }));
        assert!(!response.matches_request(&KeyServiceMessage {
            stream_id: Uuid::new_v4(),
            ..request.clone()
        }));
        assert!(!response.matches_request(&KeyServiceMessage {
            topic_id: Uuid::new_v4(),
            ..request.clone()
        }));
        response.payload = KeyServicePayload::Reject(KeyResponseTarget {
            key_uuid: Uuid::new_v4(),
            ..target.clone()
        });
        assert!(!response.matches_request(&request));
        response.payload = KeyServicePayload::Reject(KeyResponseTarget {
            recipient_device_id: "device-C".to_owned(),
            ..target
        });
        assert!(!response.matches_request(&request));
    }
}
