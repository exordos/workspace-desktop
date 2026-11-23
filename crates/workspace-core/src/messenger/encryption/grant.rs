//! Читает и записывает конверт выдачи ключа для конкретного запроса устройства.
//! HPKE обрабатывает секрет, а этот модуль задаёт формат Workspace и его контекст.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use hpke::{
    Deserializable, Kem, OpModeR, OpModeS, Serializable,
    aead::{AeadTag, ChaCha20Poly1305},
    inout::InOutBuf,
    kdf::HkdfSha256,
    kem::X25519HkdfSha256,
};
use rand::{SeedableRng, rngs::StdRng};
use ring::rand::{SecureRandom, SystemRandom};
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{GroupKey, KEY_BYTES, TAG_BYTES};

const GRANT_MAGIC: &[u8; 5] = b"WSCG\x01";
const ENCAPSULATED_KEY_BYTES: usize = 32;
const GRANT_BYTES: usize = GRANT_MAGIC.len() + ENCAPSULATED_KEY_BYTES + KEY_BYTES + TAG_BYTES;

/// Причина, по которой не получилось подготовить или открыть выдачу ключа.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum GrantError {
    #[error("system randomness is unavailable")]
    RandomnessUnavailable, // Системный генератор временно не может выдать случайные байты.
    #[error("key grant device identifiers are missing or invalid")]
    InvalidContext, // Отправитель или получатель не задан.
    #[error("invalid key request public or private key")]
    InvalidRequestKey, // Ключ запроса не имеет нужного формата или не подходит для HPKE.
    #[error("invalid encrypted key grant")]
    InvalidEnvelope, // Конверт повреждён либо создан для другого запроса или устройства.
}

/// Сведения, с которыми оба устройства связывают выдачу ключа.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GrantContext<'a> {
    pub stream_id: Uuid,              // Чат, которому принадлежит выдаваемый ключ.
    pub key_id: Uuid,                 // Поколение ключа, о котором попросило устройство.
    pub request_id: Uuid,             // Публичный UUID размещённого сообщения с запросом.
    pub sender_device_id: &'a str,    // Идентификатор устройства, выдавшего ключ.
    pub recipient_device_id: &'a str, // Идентификатор устройства, создавшего запрос.
}

/// Публичный X25519-ключ одного запроса. Его можно передавать через сервер.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RequestPublicKey([u8; KEY_BYTES]); // Байты ключа без Base64.

impl RequestPublicKey {
    /// Читает публичный ключ запроса из стандартного Base64 с заполнением.
    pub fn from_base64(encoded: &str) -> Result<Self, GrantError> {
        if encoded.len() != KEY_BYTES.div_ceil(3) * 4 {
            return Err(GrantError::InvalidRequestKey);
        }
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|_| GrantError::InvalidRequestKey)?;
        Ok(Self(
            bytes
                .try_into()
                .map_err(|_| GrantError::InvalidRequestKey)?,
        ))
    }

    /// Готовит публичный ключ для тела сообщения `key_request`.
    pub fn to_base64(self) -> String {
        STANDARD.encode(self.0)
    }
}

/// Временная пара ключей устройства. Приватный ключ хранится до завершения запроса.
pub struct RequestKeyPair {
    private: <X25519HkdfSha256 as Kem>::PrivateKey, // Секрет, который очищается при освобождении.
    public: RequestPublicKey, // Открытая часть, которую сервер получает с запросом.
}

impl RequestKeyPair {
    /// Создаёт новую пару ключей HPKE и возвращает ошибку, если генератор недоступен.
    pub fn generate() -> Result<Self, GrantError> {
        let (private, public) = X25519HkdfSha256::gen_keypair_with_rng(&mut system_rng()?);
        Ok(Self {
            private,
            public: RequestPublicKey(public.to_bytes().into()),
        })
    }

    /// Восстанавливает пару из приватного ключа, сохранённого в системном хранилище.
    pub fn from_private_bytes(bytes: &[u8]) -> Result<Self, GrantError> {
        let private = <X25519HkdfSha256 as Kem>::PrivateKey::from_bytes(bytes)
            .map_err(|_| GrantError::InvalidRequestKey)?;
        let public = RequestPublicKey(X25519HkdfSha256::sk_to_pk(&private).to_bytes().into());
        Ok(Self { private, public })
    }

    /// Даёт публичный ключ для запроса или сверки ответа сервера.
    pub fn public_key(&self) -> RequestPublicKey {
        self.public
    }

    /// Готовит очищаемые байты приватного ключа для защищённого хранилища.
    pub fn private_bytes(&self) -> Zeroizing<[u8; KEY_BYTES]> {
        let mut bytes = Zeroizing::new([0; KEY_BYTES]);
        self.private.write_exact(bytes.as_mut());
        bytes
    }
}

/// Шифрует ключ чата для устройства, создавшего указанный запрос.
pub fn seal_group_key(
    key: &GroupKey,
    recipient: RequestPublicKey,
    context: GrantContext<'_>,
) -> Result<String, GrantError> {
    let public = <X25519HkdfSha256 as Kem>::PublicKey::from_bytes(&recipient.0)
        .map_err(|_| GrantError::InvalidRequestKey)?;
    let (info, aad) = grant_inputs(context, recipient)?;
    let mut envelope = Zeroizing::new([0; GRANT_BYTES]);
    let (header, bytes) = envelope.split_at_mut(GRANT_MAGIC.len());
    header.copy_from_slice(GRANT_MAGIC);
    let (encapsulated, content) = bytes.split_at_mut(ENCAPSULATED_KEY_BYTES);
    let (ciphertext, auth_tag) = content.split_at_mut(KEY_BYTES);
    let buffer =
        InOutBuf::new(key.as_bytes(), ciphertext).map_err(|_| GrantError::InvalidEnvelope)?;
    let (encapped, tag) = hpke::single_shot_seal_inout_detached_with_rng::<
        ChaCha20Poly1305,
        HkdfSha256,
        X25519HkdfSha256,
    >(
        &OpModeS::Base,
        &public,
        &info,
        buffer,
        &aad,
        &mut system_rng()?,
    )
    .map_err(|_| GrantError::InvalidRequestKey)?;
    encapped.write_exact(encapsulated);
    tag.write_exact(auth_tag);
    Ok(STANDARD.encode(envelope.as_ref()))
}

/// Даёт HPKE библиотечный генератор со свежим системным seed и явной ошибкой вместо panic.
fn system_rng() -> Result<StdRng, GrantError> {
    let mut seed = Zeroizing::new([0; KEY_BYTES]);
    SystemRandom::new()
        .fill(seed.as_mut())
        .map_err(|_| GrantError::RandomnessUnavailable)?;
    Ok(StdRng::from_seed(*seed))
}

/// Открывает только выдачу к этому запросу и сохранённой паре ключей получателя.
pub fn open_group_key(
    pair: &RequestKeyPair,
    context: GrantContext<'_>,
    encoded: &str,
) -> Result<GroupKey, GrantError> {
    let invalid = GrantError::InvalidEnvelope;
    let envelope = decode_grant_envelope(encoded).ok_or(invalid)?;
    let (encapsulated, content) = envelope[GRANT_MAGIC.len()..].split_at(ENCAPSULATED_KEY_BYTES);
    let (ciphertext, auth_tag) = content.split_at(KEY_BYTES);
    let encapsulated =
        <X25519HkdfSha256 as Kem>::EncappedKey::from_bytes(encapsulated).map_err(|_| invalid)?;
    let tag = AeadTag::<ChaCha20Poly1305>::from_bytes(auth_tag).map_err(|_| invalid)?;
    let (info, aad) = grant_inputs(context, pair.public)?;
    let mut plaintext = Zeroizing::new([0; KEY_BYTES]);
    let buffer = InOutBuf::new(ciphertext, plaintext.as_mut()).map_err(|_| invalid)?;
    hpke::single_shot_open_inout_detached::<ChaCha20Poly1305, HkdfSha256, X25519HkdfSha256>(
        &OpModeR::Base,
        &pair.private,
        &encapsulated,
        &info,
        buffer,
        &aad,
        &tag,
    )
    .map_err(|_| invalid)?;
    Ok(GroupKey::from_bytes(*plaintext))
}

/// Проверяет только формат конверта. Без приватного ключа получателя его нельзя открыть.
pub(super) fn decode_grant_envelope(encoded: &str) -> Option<Vec<u8>> {
    if encoded.len() != GRANT_BYTES.div_ceil(3) * 4 {
        return None;
    }
    let envelope = STANDARD.decode(encoded).ok()?;
    (envelope.len() == GRANT_BYTES && envelope.starts_with(GRANT_MAGIC)).then_some(envelope)
}

/// Готовит точный JSON-контекст Electron и дополнительные данные, защищённые тегом HPKE.
fn grant_inputs(
    context: GrantContext<'_>,
    public: RequestPublicKey,
) -> Result<(Vec<u8>, Vec<u8>), GrantError> {
    if context.sender_device_id.is_empty() || context.recipient_device_id.is_empty() {
        return Err(GrantError::InvalidContext);
    }
    let info = serde_json::to_vec(&(
        "workspace-chat-crypto-v1",
        "grant",
        context.stream_id,
        context.key_id,
        context.request_id,
        context.sender_device_id,
        context.recipient_device_id,
    ))
    .map_err(|_| GrantError::InvalidContext)?;
    let aad = [info.as_slice(), GRANT_MAGIC.as_slice(), &public.0].concat();
    Ok((info, aad))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Вектор получен через те же @hpke/core 1.9.0, chacha20poly1305 и dhkem-x25519 1.8.0, что использует Electron af099988.
    const PRIVATE: &str = "PKphvBPlZHPpE6hcM89NYDrJmlF+6pXtRXPncrZENfc=";
    const PUBLIC: &str = "aTZYJUYw9zrY2nj7Mxv5ds1C+Q4OnJ6D9AxRBypvdBc=";
    const ENVELOPE: &str = "V1NDRwFfUEleSkAb3jZfkSapKN3Q8BAB61E8u4xE2eK8mNJ6T4EpS5XyMw2tkda7x9Z110r9Itm9tpFabnuCUajJ+foBrrPTg7mdAhp+J1El9UrBtA==";

    /// Возвращает исходный контекст выдачи из синтетического вектора Electron.
    fn context() -> GrantContext<'static> {
        GrantContext {
            stream_id: uuid::uuid!("956f084e-8992-4334-a6de-fe84ac055e46"),
            key_id: uuid::uuid!("e70f0d95-6784-4125-8dfd-83eac755cecf"),
            request_id: uuid::uuid!("27273c8a-686c-4aaa-a866-84f080855aa2"),
            sender_device_id: "Electron/Компьютер:\"А\"",
            recipient_device_id: "desktop-device-1",
        }
    }

    /// Возвращает известное содержимое синтетической выдачи.
    fn expected_secret() -> [u8; KEY_BYTES] {
        std::array::from_fn(|index| index as u8)
    }

    /// Открывает готовый конверт Electron и сверяет публичную часть восстановленного секрета.
    #[test]
    fn opens_pinned_electron_vector() {
        let pair = RequestKeyPair::from_private_bytes(&STANDARD.decode(PRIVATE).unwrap()).unwrap();
        assert_eq!(
            pair.public_key(),
            RequestPublicKey::from_base64(PUBLIC).unwrap()
        );
        assert_eq!(
            open_group_key(&pair, context(), ENVELOPE)
                .unwrap()
                .as_bytes(),
            &expected_secret()
        );
    }

    /// Тестирует запись, перезапуск, повтор с новым конвертом и точный формат байтов.
    #[test]
    fn restores_request_key_and_opens_issued_grant() {
        let original = RequestKeyPair::generate().unwrap();
        let restored =
            RequestKeyPair::from_private_bytes(original.private_bytes().as_ref()).unwrap();
        let public = RequestPublicKey::from_base64(&original.public_key().to_base64()).unwrap();
        let secret = GroupKey::from_bytes(expected_secret());
        let first = seal_group_key(&secret, public, context()).unwrap();
        let second = seal_group_key(&secret, public, context()).unwrap();
        assert_ne!(first, second);
        for grant in [&first, &second] {
            let bytes = STANDARD.decode(grant).unwrap();
            assert_eq!(bytes.len(), GRANT_BYTES);
            assert!(bytes.starts_with(GRANT_MAGIC));
            assert_eq!(
                open_group_key(&restored, context(), grant)
                    .unwrap()
                    .as_bytes(),
                secret.as_bytes()
            );
        }
    }

    /// Блокирует подмену параметров выдачи, версии, тега и пары ключей устройства.
    #[test]
    fn rejects_different_request_context_and_corrupt_envelope() {
        let pair = RequestKeyPair::from_private_bytes(&STANDARD.decode(PRIVATE).unwrap()).unwrap();
        for changed in [
            GrantContext {
                stream_id: Uuid::new_v4(),
                ..context()
            },
            GrantContext {
                key_id: Uuid::new_v4(),
                ..context()
            },
            GrantContext {
                request_id: Uuid::new_v4(),
                ..context()
            },
            GrantContext {
                sender_device_id: "sender-2",
                ..context()
            },
            GrantContext {
                recipient_device_id: "recipient-2",
                ..context()
            },
        ] {
            assert!(matches!(
                open_group_key(&pair, changed, ENVELOPE),
                Err(GrantError::InvalidEnvelope)
            ));
        }
        assert!(matches!(
            open_group_key(&RequestKeyPair::generate().unwrap(), context(), ENVELOPE),
            Err(GrantError::InvalidEnvelope)
        ));
        let envelope = STANDARD.decode(ENVELOPE).unwrap();
        for index in [GRANT_MAGIC.len() - 1, GRANT_BYTES - 1] {
            let mut mutated = envelope.clone();
            mutated[index] ^= 1;
            assert!(matches!(
                open_group_key(&pair, context(), &STANDARD.encode(mutated)),
                Err(GrantError::InvalidEnvelope)
            ));
        }
        assert!(matches!(
            open_group_key(&pair, context(), ENVELOPE.trim_end_matches('=')),
            Err(GrantError::InvalidEnvelope)
        ));
        assert!(RequestPublicKey::from_base64(PUBLIC.trim_end_matches('=')).is_err());
        assert!(matches!(
            seal_group_key(
                &GroupKey::from_bytes(expected_secret()),
                RequestPublicKey([0; KEY_BYTES]),
                context()
            ),
            Err(GrantError::InvalidRequestKey)
        ));
    }
}
