//! Выполняет один multipart POST файла в Workspace и сверяет квитанцию с проверенным запросом.

use reqwest::multipart::{Form, Part};

use crate::messenger::files::{
    UploadAttachmentError, UploadedAttachment,
    upload::{ExpectedAttachment, PreparedAttachment},
};

use super::*;

const FILES_PATH: &str = "/api/workspace/v1/messenger/files/";
const RESPONSE_LIMIT: usize = 64 * 1024;
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
    ) -> Result<UploadedAttachment, UploadAttachmentError> {
        let mut url = workspace_origin(owner).map_err(|_| UploadAttachmentError::InvalidInput)?;
        url.set_path(FILES_PATH);
        let PreparedAttachment { bytes, receipt } = prepared;
        let file = Part::bytes(bytes)
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
        Ok(receipt.reference(id))
    }
}
