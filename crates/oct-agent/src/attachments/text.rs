//! Frozen text-attachment snapshots. Framing uses logical resource ids so
//! moving the data directory never changes a persisted model-visible prefix.
use super::{AttachmentService, ServiceError, normalize::image_mime};
use crate::attachment_types::{
    AttachmentDto, MessageAttachment, TextSnapshot, UserContent, attachment_url,
};
use crate::db::attachments::AttachmentRow;

/// 单个文本附件的字节硬顶；此外所有文本附件共享模型窗口 10% 的 token
/// 预算（封顶 32k，未知窗口用 16k）。超出部分附逻辑引用，模型可自行读取。
pub const TEXT_INLINE_MAX_BYTES: usize = 256 * 1024;
pub fn text_inline_token_budget(window: Option<u32>) -> u64 {
    window
        .map(|window| (u64::from(window) / 10).min(32_000))
        .unwrap_or(16_000)
}

// Match the context estimator's ASCII-quarter / non-ASCII-full heuristic,
// rounding up so splitting a budget across files cannot lose token fractions.
fn tokens(text: &str) -> u64 {
    text.chars()
        .map(|c| if c.is_ascii() { 1u64 } else { 4 })
        .sum::<u64>()
        .div_ceil(4)
}
fn header(row: &AttachmentRow) -> String {
    format!("[attachment: {}]\n", row.filename)
}
fn note(row: &AttachmentRow, cut: usize) -> String {
    format!(
        "\n[truncated at {cut} bytes; full file: {}]",
        attachment_url(&row.asset_id)
    )
}
fn framing(row: &AttachmentRow) -> u64 {
    tokens(&header(row)) + tokens(&note(row, TEXT_INLINE_MAX_BYTES))
}

/// Reserve every filename/reference first, then share the remaining budget.
/// A large first file cannot crowd every later attachment out of the prompt.
/// Very small windows retain the minimum full-file hints without body space.
pub(super) async fn build_content(
    service: &AttachmentService,
    rows: &[AttachmentRow],
    body: &str,
    window: Option<u32>,
) -> Result<UserContent, ServiceError> {
    let mut text_files = rows
        .iter()
        .filter(|row| image_mime(&row.mime_type).is_none())
        .count() as u64;
    let frame: u64 = rows
        .iter()
        .filter(|row| image_mime(&row.mime_type).is_none())
        .map(framing)
        .sum();
    let mut remaining = text_inline_token_budget(window).saturating_sub(frame);
    let mut attachments = Vec::with_capacity(rows.len());
    for row in rows {
        let snapshot = if image_mime(&row.mime_type).is_some() {
            None
        } else {
            let budget = framing(row) + remaining / text_files;
            let bytes = service.read_file(row).await.map_err(|error| {
                ServiceError::Invalid(format!(
                    "无法读取附件「{}」，请重试或重新上传：{error}",
                    row.filename
                ))
            })?;
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| ServiceError::Invalid("附件不再是有效 UTF-8 文本".into()))?;
            let prefix = header(row);
            let snapshot = if bytes.len() <= TEXT_INLINE_MAX_BYTES
                && tokens(&prefix) + tokens(text) <= budget
            {
                TextSnapshot {
                    text: format!("{prefix}{text}"),
                    truncated: false,
                    included_bytes: bytes.len() as u64,
                }
            } else {
                let body_units = budget.saturating_sub(framing(row)).saturating_mul(4);
                let mut used = 0;
                let mut cut = 0;
                for (index, c) in text.char_indices() {
                    let end = index + c.len_utf8();
                    let units = if c.is_ascii() { 1 } else { 4 };
                    if end > TEXT_INLINE_MAX_BYTES || used + units > body_units {
                        break;
                    }
                    used += units;
                    cut = end;
                }
                TextSnapshot {
                    text: format!("{prefix}{}{}", &text[..cut], note(row, cut)),
                    truncated: true,
                    included_bytes: cut as u64,
                }
            };
            remaining = (remaining + framing(row)).saturating_sub(tokens(&snapshot.text));
            text_files -= 1;
            Some(snapshot)
        };
        attachments.push(MessageAttachment {
            asset: AttachmentDto {
                id: row.asset_id.clone(),
                filename: row.filename.clone(),
                mime_type: row.mime_type.clone(),
                size_bytes: row.size_bytes,
            },
            snapshot,
        });
    }
    Ok(UserContent {
        body: body.into(),
        attachments,
    })
}
