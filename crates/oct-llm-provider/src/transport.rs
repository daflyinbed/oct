//! Shared transport plumbing for provider adapters: HTTP response status →
//! `ModelError` classification and byte-level SSE event framing. Each
//! protocol keeps its own wire types, message mapping and event state
//! machine; this module only owns what is byte-identical between them.

use crate::core::ModelError;

/// Map a provider HTTP response's status to the common error kinds (401 →
/// `Authentication`, 429 → `RateLimited`, other failures → `Provider` with
/// status and body). On success the response is handed back for the
/// protocol-specific body handling.
pub(crate) async fn check_response_status(
    response: reqwest::Response,
    provider_label: &str,
) -> Result<reqwest::Response, ModelError> {
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(ModelError::Authentication);
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(ModelError::RateLimited);
    }
    if !status.is_success() {
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<failed to read error body>".to_string());
        return Err(ModelError::provider(format!(
            "{provider_label} returned {status}: {body}"
        )));
    }
    Ok(response)
}

/// Byte-level SSE event framing shared by every streaming provider.
///
/// Network chunks routinely split a multibyte UTF-8 character mid-sequence,
/// so decoding whole chunks directly can fail; instead only complete events
/// (bounded by the blank line) are decoded. A multibyte UTF-8 character
/// split across network chunks can never be cut inside an event here
/// because UTF-8 continuation bytes never collide with \n / \r.
#[derive(Debug, Default)]
pub(crate) struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Feed one network chunk, returning every event it completed. Each
    /// returned event is one framed SSE block (all its lines, separator
    /// stripped), decoded as UTF-8.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Result<Vec<String>, ModelError> {
        self.buffer.extend_from_slice(chunk);
        self.drain_complete()
    }

    /// Flush the tail at stream EOF: a whitespace-only remainder is dropped,
    /// anything else is delivered as one final (truncated) event so streams
    /// cut off mid-event still surface what arrived.
    pub(crate) fn finish(&mut self) -> Result<Vec<String>, ModelError> {
        if self.buffer.iter().all(|b| b.is_ascii_whitespace()) {
            self.buffer.clear();
            return Ok(Vec::new());
        }
        let remainder = decode_sse_event(std::mem::take(&mut self.buffer))?;
        Ok(vec![remainder])
    }

    fn drain_complete(&mut self) -> Result<Vec<String>, ModelError> {
        let mut events = Vec::new();
        while let Some(event) = take_sse_event(&mut self.buffer) {
            events.push(decode_sse_event(event)?);
        }
        Ok(events)
    }
}

/// Find `needle` in `haystack` at the byte level.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Extract the next complete SSE event from `buffer`, byte level. `\n\n`
/// and `\r\n\r\n` terminators are both accepted in any order; when both
/// could match, the earlier byte offset wins.
fn take_sse_event(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    let lf = find_subslice(buffer, b"\n\n");
    let crlf = find_subslice(buffer, b"\r\n\r\n");
    let (index, sep_len) = match (lf, crlf) {
        (Some(lf), Some(crlf)) if crlf < lf => (crlf, 4),
        (Some(lf), _) => (lf, 2),
        (None, Some(crlf)) => (crlf, 4),
        (None, None) => return None,
    };
    let event = buffer[..index].to_vec();
    buffer.drain(..index + sep_len);
    Some(event)
}

fn decode_sse_event(bytes: Vec<u8>) -> Result<String, ModelError> {
    String::from_utf8(bytes)
        .map_err(|err| ModelError::transport(format!("stream event was not valid utf-8: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_events(chunks: &[&[u8]]) -> Vec<String> {
        let mut decoder = SseDecoder::new();
        let mut events = Vec::new();
        for chunk in chunks {
            events.extend(decoder.push(chunk).unwrap());
        }
        events.extend(decoder.finish().unwrap());
        events
    }

    /// Feed the transcript through every possible chunk split: framing must
    /// never depend on where the network happened to cut, including inside
    /// a multibyte UTF-8 character.
    #[test]
    fn framing_survives_every_chunk_split() {
        let transcript: &[u8] = concat!(
            "event: content_block_delta\n",
            "data: {\"text\":\"你好\"}\n\n",
            "data: {\"text\":\"，世界！\"}\n\n",
            "event: message_stop\n",
            "data: {}\n\n",
        )
        .as_bytes();

        let expected = all_events(&[transcript]);
        assert_eq!(
            expected,
            vec![
                "event: content_block_delta\ndata: {\"text\":\"你好\"}",
                "data: {\"text\":\"，世界！\"}",
                "event: message_stop\ndata: {}",
            ]
        );

        for size in 1..transcript.len() {
            let chunks: Vec<&[u8]> = transcript.chunks(size).collect();
            assert_eq!(
                all_events(&chunks),
                expected,
                "mismatch at chunk size {size}"
            );
        }
    }

    /// Terminators may be LF-only, CRLF-only, or mixed within one stream,
    /// in any order; a `\n\n` inside a CRLF stream (or vice versa) must not
    /// split an event early.
    #[test]
    fn framing_accepts_mixed_lf_and_crlf_terminators_in_any_order() {
        // CRLF event first, then LF, then CRLF again, plus a separator that
        // mixes both styles (`\r\n\n`). The `\r` of a mixed separator rides
        // along inside the event; consumers parse with `.lines()`, which
        // strips it.
        let transcript = concat!(
            "event: one\r\n",
            "data: first\r\n",
            "\r\n",
            "data: second\n",
            "\n",
            "data: third\r\n",
            "\n",
            "event: four\r\n",
            "data: fourth\r\n",
            "\r\n",
        );
        assert_eq!(
            all_events(&[transcript.as_bytes()]),
            vec![
                "event: one\r\ndata: first",
                "data: second",
                "data: third\r",
                "event: four\r\ndata: fourth",
            ]
        );
    }

    /// EOF with a truncated (non-whitespace) remainder delivers it as one
    /// final event; EOF on whitespace only delivers nothing.
    #[test]
    fn eof_tail_frame_is_delivered_only_when_non_whitespace() {
        let truncated = b"data: {\"text\":\"partial";
        assert_eq!(all_events(&[truncated]), vec!["data: {\"text\":\"partial"]);

        let whitespace = b"data: ok\n\n  \n\t\r\n";
        assert_eq!(all_events(&[whitespace]), vec!["data: ok"]);
    }

    /// A chunk boundary between `\r` and `\n` of a CRLF terminator must not
    /// trick the framer into emitting an event early or losing bytes.
    #[test]
    fn framing_survives_split_inside_crlf_terminator() {
        let raw = b"data: one\r\n\r\ndata: two\r\n\r\n";
        // Cut after the first "\r" of the blank line: offset of b"\r" is 9.
        let cut = 10;
        let chunks: Vec<&[u8]> = vec![&raw[..cut], &raw[cut..]];
        assert_eq!(all_events(&chunks), vec!["data: one", "data: two"]);
    }
}
