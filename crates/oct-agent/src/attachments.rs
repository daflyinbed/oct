//! Attachment lifecycle, immutable bytes and request-side materialization.
//! Neutral message contracts live in `attachment_types`; HTTP code only adapts
//! requests to this service. Locks and database/file publication stay internal.
pub mod normalize;
mod request;
mod service;
mod store;
pub use request::RequestAssembler;
mod text;
pub use crate::attachment_types::{
    ATTACHMENT_URL_PREFIX, AttachmentDto, attachment_url, parse_attachment_id,
};
pub use service::{AttachmentService, PreparedUserMessage, ServiceError, UploadGuard};
pub use text::{TEXT_INLINE_MAX_BYTES, text_inline_token_budget};
/// Single-file limit. The route also reserves multipart header/boundary
/// overhead so a file exactly at the limit can still reach this validation.
pub const MAX_ATTACHMENT_BYTES: i64 = 10 * 1024 * 1024;
pub const MULTIPART_OVERHEAD_SLACK: u64 = 64 * 1024;
/// Maximum upload tickets claimed by one user message.
pub const MAX_ATTACHMENTS_PER_MESSAGE: usize = 8;
