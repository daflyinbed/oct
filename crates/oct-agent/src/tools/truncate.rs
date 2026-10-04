//! Helpers to keep tool output within a size budget so the LLM context is not
//! flooded by huge command outputs, file dumps, or directory listings.
//!
//! Two strategies are provided:
//! - [`truncate_middle`]: drop the middle of a large blob, keeping head and tail.
//! - [`truncate_line`]: cap a single overly long line, keeping its head.

use std::collections::VecDeque;

/// Maximum size of a tool's final combined output, in bytes.
pub const MAX_TOOL_OUTPUT_BYTES: usize = 50_000;

/// Maximum length of a single output line, in chars.
pub const MAX_LINE_CHARS: usize = 2_000;

/// Streaming equivalent of `truncate_middle`: retain only a fixed head/tail
/// window, even when the source has millions of lines.
pub(crate) struct BoundedText {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: usize,
    max_bytes: usize,
}

impl BoundedText {
    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            head: Vec::new(),
            tail: VecDeque::new(),
            total: 0,
            max_bytes,
        }
    }

    pub(crate) fn push(&mut self, text: &str) {
        self.total += text.len();
        let bytes = text.as_bytes();
        let head_len = bytes.len().min(self.max_bytes / 2 - self.head.len());
        self.head.extend_from_slice(&bytes[..head_len]);
        let bytes = &bytes[head_len..];
        let tail_budget = self.max_bytes - self.max_bytes / 2;
        if bytes.len() >= tail_budget {
            self.tail.clear();
            self.tail.extend(&bytes[bytes.len() - tail_budget..]);
        } else {
            let excess = (self.tail.len() + bytes.len()).saturating_sub(tail_budget);
            self.tail.drain(..excess);
            self.tail.extend(bytes);
        }
    }

    pub(crate) fn finish(mut self) -> (String, bool) {
        if self.total <= self.max_bytes {
            self.head.extend(self.tail);
            return (String::from_utf8(self.head).expect("UTF-8 input"), false);
        }
        // The byte windows can cut inside a character. Shrink each cut to a
        // character boundary, just like truncate_middle.
        let head_end = match std::str::from_utf8(&self.head) {
            Ok(_) => self.head.len(),
            Err(error) => error.valid_up_to(),
        };
        self.head.truncate(head_end);
        while self.tail.front().is_some_and(|byte| byte & 0xC0 == 0x80) {
            self.tail.pop_front();
        }
        let omitted = self.total - self.head.len() - self.tail.len();
        let head = String::from_utf8(self.head).expect("UTF-8 head");
        let tail = String::from_utf8(self.tail.into_iter().collect()).expect("UTF-8 tail");
        (
            format!(
                "[output truncated: original size {} bytes]\n{head}\n\n... {omitted} bytes omitted ...\n\n{tail}",
                self.total,
            ),
            true,
        )
    }
}

/// Truncate `s` to at most `max_bytes` of content by dropping its middle.
///
/// The first `max_bytes / 2` bytes and the last `max_bytes - max_bytes / 2` bytes
/// are kept, both floored to UTF-8 char boundaries (never mid-char), and joined by
/// a `... N bytes omitted ...` marker. A header line with the original size is
/// prepended so the model sees the truncation signal immediately. All arithmetic
/// is on byte lengths; the kept content (excluding header and marker) stays within
/// `max_bytes`.
pub fn truncate_middle(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }

    let total = s.len();
    let head_budget = max_bytes / 2;
    let tail_budget = max_bytes - head_budget;

    // Floor the head cut down to a char boundary: walk back until byte `head_end`
    // no longer splits a multi-byte char. 0 is always a boundary, so this stops.
    let mut head_end = head_budget;
    while !s.is_char_boundary(head_end) {
        head_end -= 1;
    }

    // Floor the tail cut to a char boundary: walking the start forward shrinks the
    // kept tail instead of growing it past the budget. `total` is always a
    // boundary, so this stops too.
    let mut tail_start = total - tail_budget;
    while !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }

    let head = &s[..head_end];
    let tail = &s[tail_start..];
    let omitted = tail_start - head_end;

    let mut out = format!("[output truncated: original size {total} bytes]\n");
    out.push_str(head);
    out.push_str("\n\n... ");
    out.push_str(&omitted.to_string());
    out.push_str(" bytes omitted ...\n\n");
    out.push_str(tail);
    out
}

/// Cap a single line to `max_chars` characters.
///
/// Trailing `\n`/`\r` do not count toward the cap. An over-long line is cut on a
/// char boundary after `max_chars` chars and suffixed with `[...truncated]`.
pub fn truncate_line(line: &str, max_chars: usize) -> String {
    let content = line.trim_end_matches(['\n', '\r']);
    if content.chars().count() <= max_chars {
        return line.to_string();
    }
    let head: String = content.chars().take(max_chars).collect();
    format!("{head}[...truncated]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_truncation_preserves_utf8_across_pushes() {
        for text in ["short".to_string(), "中👍末abc".repeat(100)] {
            let mut output = BoundedText::new(101);
            for character in text.chars() {
                output.push(&character.to_string());
            }
            let (output, truncated) = output.finish();
            assert_eq!(output, truncate_middle(&text, 101));
            assert_eq!(truncated, text.len() > 101);
        }
    }

    #[test]
    fn truncate_middle_passthrough_when_under_budget() {
        let short = "hello world";
        assert_eq!(truncate_middle(short, MAX_TOOL_OUTPUT_BYTES), short);

        // Exactly at the budget is also returned unchanged.
        let exact = "a".repeat(100);
        assert_eq!(truncate_middle(&exact, 100), exact);
    }

    #[test]
    fn truncate_middle_keeps_head_and_tail_with_marker() {
        let head = "H".repeat(50);
        let tail = "T".repeat(50);
        let s = format!("{head}{}{tail}", "m".repeat(1000));
        assert_eq!(s.len(), 1100);

        let out = truncate_middle(&s, 100);

        assert!(out.starts_with("[output truncated: original size 1100 bytes]\n"));
        // Both halves fit their budgets (50 + 50), so head and tail survive intact.
        assert!(out.contains(&head));
        assert!(out.contains(&tail));
        assert!(out.contains("\n\n... 1000 bytes omitted ...\n\n"));
        assert!(out.len() < s.len());
    }

    #[test]
    fn truncate_middle_keeps_content_within_budget() {
        let s = "x".repeat(10_000);
        let out = truncate_middle(&s, 100);
        // 50 kept head bytes + 50 kept tail bytes, nothing else in content.
        assert_eq!(out.matches('x').count(), 100);
    }

    #[test]
    fn truncate_middle_is_utf8_boundary_safe_with_cjk() {
        // 3-byte chars at both cut points: 100x 中 (300B) + 100x 👍 (400B) + 20x 末 (60B).
        let s = format!(
            "{}{}{}",
            "中".repeat(100),
            "👍".repeat(100),
            "末".repeat(20)
        );
        assert_eq!(s.len(), 760);

        let out = truncate_middle(&s, 100);

        // Slicing must never split a char or synthesize U+FFFD inside kept regions.
        assert!(!out.contains('\u{FFFD}'));
        assert!(out.len() < s.len());
        assert!(out.starts_with("[output truncated: original size 760 bytes]\n"));

        // Head budget 50 floors to byte 48 (16x 中); tail budget 50 floors the tail
        // start to byte 712, keeping 48 bytes (16x 末).
        assert!(out.contains(&"中".repeat(16)));
        assert!(out.contains(&"末".repeat(16)));
        // omitted = 712 - 48 (differs from the naive 760 - 100 due to boundary flooring)
        assert!(out.contains("\n\n... 664 bytes omitted ...\n\n"));
    }

    #[test]
    fn truncate_middle_is_utf8_boundary_safe_with_emoji() {
        // 4-byte emoji straddling both cut points.
        let s = "👍".repeat(500);
        assert_eq!(s.len(), 2000);

        let out = truncate_middle(&s, 100);

        assert!(!out.contains('\u{FFFD}'));
        assert!(out.len() < s.len());
        // Head floors 50 -> 48 (12 emoji); tail start floors 1950 -> 1952 (12 emoji).
        assert!(out.contains(&"👍".repeat(12)));
        assert!(out.contains("\n\n... 1904 bytes omitted ...\n\n"));
    }

    #[test]
    fn truncate_line_passthrough() {
        assert_eq!(truncate_line("hello", MAX_LINE_CHARS), "hello");
        assert_eq!(truncate_line("", 10), "");

        let exact = "ab".repeat(5); // 10 chars, exactly at the cap
        assert_eq!(truncate_line(&exact, 10), exact);
    }

    #[test]
    fn truncate_line_ignores_trailing_newline_in_budget() {
        let line = format!("{}\n", "a".repeat(10));
        assert_eq!(truncate_line(&line, 10), line);
    }

    #[test]
    fn truncate_line_keeps_head_and_marks_truncation() {
        let line = "x".repeat(2500);
        let out = truncate_line(&line, 2000);
        assert!(out.starts_with(&"x".repeat(2000)));
        assert!(out.ends_with("[...truncated]"));
        assert_eq!(out.chars().count(), 2000 + "[...truncated]".chars().count());
    }

    #[test]
    fn truncate_line_is_utf8_boundary_safe() {
        let line = "中".repeat(3000);
        let out = truncate_line(&line, 2000);
        // The cut lands between whole chars, never mid-char.
        assert!(out.starts_with(&"中".repeat(2000)));
        assert!(out.ends_with("[...truncated]"));
        assert!(!out.contains('\u{FFFD}'));
    }
}
