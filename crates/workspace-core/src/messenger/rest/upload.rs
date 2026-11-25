//! Передаёт файл через multipart, сверяет квитанцию и отправляет отдельный запрос очистки.

use std::convert::Infallible;

use futures::stream;
use reqwest::{
    Body,
    multipart::{Form, Part},
};
use tokio::sync::watch;

use crate::messenger::files::{
    AttachmentUploadProgress, DiscardAttachmentError, UploadAttachmentError,
    upload::{ExpectedAttachment, PreparedAttachment},
};

use super::*;

const FILES_PATH: &str = "/api/workspace/v1/messenger/files/";
const RESPONSE_LIMIT: usize = 64 * 1024;
const FILE_CHUNK_BYTES: usize = 64 * 1024;
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// Подтверждение записи от файлового API в контексте загружавшего пользователя.
#[derive(Deserialize)]
struct UploadReceipt {
    uuid: Uuid,                 // UUID нового файла, создаёт сервер.
    user_uuid: String,          // UUID пользователя, которому принадлежит файл.
    project_id: Option<String>, // Сравниваем проект, если API его раскрывает.
    stream_uuid: Uuid,          // Стрим, члены которого имеют право на скачивание.
    name: String,               // Имя из multipart.
    content_type: String,       // MIME части файла.
    size_bytes: usize,          // Полученный сервером размер файла.
    hash: String,               // SHA-256, вычисленный по загруженным байтам.
}

impl UploadReceipt {
    /// Принимает квитанцию только для исходных владельца, стрима, имени, формата и байтов.
    fn verified_id(
        &self,
        owner: &OwnerContext,
        stream: Uuid,
        expected: &ExpectedAttachment,
    ) -> Option<Uuid> {
        (!self.uuid.is_nil()
            && self.stream_uuid == stream
            && matches_owner(owner, self.project_id.as_deref(), Some(&self.user_uuid))
            && self.name == expected.name
            && self.content_type.eq_ignore_ascii_case(&expected.mime)
            && self.size_bytes == expected.size
            && self.hash.eq_ignore_ascii_case(&expected.sha256))
        .then_some(self.uuid)
    }
}

impl RestClient {
    /// Отправляет ровно один запрос без ACL public и ограничивает размер квитанции до разбора JSON.
    pub(in crate::messenger) async fn multipart_attachment(
        &self,
        owner: &OwnerContext,
        token: &str,
        stream: Uuid,
        prepared: PreparedAttachment,
        progress: Option<watch::Sender<AttachmentUploadProgress>>,
    ) -> Result<(Uuid, String), UploadAttachmentError> {
        let mut url = workspace_origin(owner).map_err(|_| UploadAttachmentError::InvalidInput)?;
        url.set_path(FILES_PATH);
        let PreparedAttachment { bytes, receipt } = prepared;
        let file = attachment_part(bytes, progress)
            .file_name(receipt.name.clone())
            .mime_str(&receipt.mime)
            .map_err(|_| UploadAttachmentError::InvalidInput)?;
        let form = Form::new()
            .text("stream_uuid", stream.to_string())
            .text("name", receipt.name.clone())
            .part("file", file);
        let mut response = self
            .send
            .post(url)
            .header(ACCEPT, "application/json")
            .bearer_auth(token)
            .multipart(form)
            .timeout(UPLOAD_TIMEOUT)
            .send()
            .await
            .map_err(|_| UploadAttachmentError::Unknown)?;
        match response.status() {
            StatusCode::OK | StatusCode::CREATED => {}
            StatusCode::PAYLOAD_TOO_LARGE => return Err(UploadAttachmentError::TooLarge),
            status if status.is_server_error() || status == StatusCode::REQUEST_TIMEOUT => {
                return Err(UploadAttachmentError::Unknown);
            }
            status => return Err(UploadAttachmentError::Rejected(status.as_u16())),
        }
        if response
            .content_length()
            .is_some_and(|bytes| bytes > RESPONSE_LIMIT as u64)
        {
            return Err(UploadAttachmentError::Unknown);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| UploadAttachmentError::Unknown)?
        {
            if chunk.len() > RESPONSE_LIMIT - body.len() {
                return Err(UploadAttachmentError::Unknown);
            }
            body.extend_from_slice(&chunk);
        }
        let reply: UploadReceipt =
            serde_json::from_slice(&body).map_err(|_| UploadAttachmentError::Unknown)?;
        let id = reply
            .verified_id(owner, stream, &receipt)
            .ok_or(UploadAttachmentError::Unknown)?;
        Ok((id, receipt.markdown(id)))
    }

    /// Запрашивает удаление проверенной записи; успех подтверждают ответы 200 и 204.
    pub(in crate::messenger) async fn delete_attachment(
        &self,
        owner: &OwnerContext,
        token: &str,
        file_id: Uuid,
    ) -> Result<(), DiscardAttachmentError> {
        let mut url = workspace_origin(owner).map_err(|_| DiscardAttachmentError::Unknown)?;
        url.set_path(&format!("{FILES_PATH}{file_id}"));
        let response = self
            .send
            .delete(url)
            .header(ACCEPT, "application/json")
            .bearer_auth(token)
            .send()
            .await
            .map_err(|_| DiscardAttachmentError::Unknown)?;
        match response.status() {
            StatusCode::OK | StatusCode::NO_CONTENT => Ok(()),
            status => Err(DiscardAttachmentError::Rejected(status.as_u16())),
        }
    }
}

/// Считает байты файла по мере чтения HTTP-клиентом, сохраняя известную длину multipart.
fn attachment_part(
    bytes: Vec<u8>,
    progress: Option<watch::Sender<AttachmentUploadProgress>>,
) -> Part {
    let Some(progress) = progress else {
        return Part::bytes(bytes);
    };
    let total_bytes = bytes.len() as u64;
    progress.send_replace(AttachmentUploadProgress {
        loaded_bytes: 0,
        total_bytes,
    });
    let chunks = stream::unfold(
        (bytes, 0, progress),
        move |(bytes, start, progress)| async move {
            if start == bytes.len() {
                return None;
            }
            let end = (start + FILE_CHUNK_BYTES).min(bytes.len());
            let chunk = bytes[start..end].to_vec();
            progress.send_replace(AttachmentUploadProgress {
                loaded_bytes: end as u64,
                total_bytes,
            });
            Some((Ok::<_, Infallible>(chunk), (bytes, end, progress)))
        },
    );
    Part::stream_with_length(Body::wrap_stream(chunks), total_bytes)
}
