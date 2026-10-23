use anyhow::{Context, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::io::{self, BufRead, BufReader, Read};
use std::sync::Arc;

use super::shared::{AgentReadState, file_mtime};
use super::truncate::{BoundedText, MAX_LINE_CHARS, MAX_TOOL_OUTPUT_BYTES, truncate_line};
use super::workspace::WorkspaceRoots;
use crate::tool_contract::AgentTool;
use crate::tool_contract::ToolContext;
use crate::tool_contract::ToolOutput;

pub struct ReadFileTool {
    roots: WorkspaceRoots,
    /// This agent's own read records (shared with its write/edit tools only
    /// — never with another agent).
    reads: Arc<AgentReadState>,
}

impl ReadFileTool {
    pub fn new(roots: WorkspaceRoots, reads: Arc<AgentReadState>) -> Self {
        Self { roots, reads }
    }
}

/// UI-only metadata for a read_file call.
#[derive(Debug, Serialize)]
struct ReadFileDetails {
    title: String,
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
    total_lines: usize,
    shown_lines: usize,
    /// Per-line or middle truncation kicked in (best effort).
    truncated: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReadFileArgs {
    /// Path to read: relative/absolute filesystem path or oct-attachment:// resource reference
    path: String,
    /// Start line number (1-indexed, inclusive). Omit to start from beginning.
    start_line: Option<usize>,
    /// End line number (1-indexed, inclusive). Omit to read until end.
    end_line: Option<usize>,
}

#[async_trait]
impl AgentTool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read the contents of a file. Returns the file content with line numbers. \
         Use start_line and end_line to read a specific range. Background command \
         output_file paths and oct-attachment:// references are readable here; prefer read_file or grep over shell commands."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(ReadFileArgs)).unwrap()
    }

    fn title(&self, args: &serde_json::Value) -> String {
        args.get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("read_file")
            .to_string()
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let args: ReadFileArgs =
            serde_json::from_value(args).context("Invalid arguments for read_file")?;

        // Resolution no longer rejects outside-workspace paths; the outside
        // classification is the permission layer's input, not this tool's.
        let resolved = match self.roots.resolve_path(&args.path) {
            Ok(resolved) => resolved.path,
            Err(output) => return Ok(output),
        };

        if !resolved.is_file() {
            return Ok(ToolOutput::error(format!("{} is not a file", args.path)));
        }

        let start = args.start_line.unwrap_or(1).max(1);
        let end = args.end_line.unwrap_or(usize::MAX);
        if end < start {
            return Ok(ToolOutput::error("end_line must be at least start_line"));
        }
        let path = resolved.clone();
        // Count and select lines without loading the whole file or allocating
        // an unbounded single line. Snapshot the byte length so a live log
        // cannot keep extending this read indefinitely.
        let preview = tokio::task::spawn_blocking(move || {
            let file = std::fs::File::open(path)?;
            let len = file.metadata()?.len();
            read_preview(BufReader::new(file.take(len)), start, end)
        })
        .await
        .context("read_file task panicked")?;
        let (content, total, shown_lines, truncated) = match preview {
            Ok(preview) => preview,
            Err(e) => return Ok(ToolOutput::error(format!("Failed to read file: {e}"))),
        };

        // Record the read (mtime at read time) so edit_file can enforce its
        // read-before-edit rule. Partial range reads count as reads too.
        if let Some(mtime) = file_mtime(&resolved) {
            self.reads.record_read(&resolved, mtime);
        }

        let end = end.min(total);

        let details = ReadFileDetails {
            title: args.path.clone(),
            path: args.path.clone(),
            start_line: args.start_line,
            end_line: args.end_line,
            total_lines: total,
            shown_lines,
            truncated,
        };

        if start > total {
            return Ok(ToolOutput::success_with_details(
                format!("(File has {total} lines, requested start_line {start} is beyond end)"),
                details,
            ));
        }

        let header = if args.start_line.is_some() || args.end_line.is_some() {
            format!("({} total lines, showing {start}-{end})\n", total)
        } else {
            String::new()
        };

        Ok(ToolOutput::success_with_details(
            format!("{header}{content}"),
            details,
        ))
    }
}

/// Read a bounded prefix of a line while consuming the rest in fixed chunks.
/// Four bytes per character plus one extra character suffice to decide the
/// existing character cap, including lossy decoding of invalid UTF-8.
fn line_prefix(reader: &mut impl BufRead, capture: bool) -> io::Result<Option<Vec<u8>>> {
    const PREFIX_BYTES: usize = (MAX_LINE_CHARS + 1) * 4;
    let mut prefix = Vec::new();
    let mut consumed = false;
    let mut discarded = false;
    loop {
        let bytes = reader.fill_buf()?;
        if bytes.is_empty() {
            if !consumed {
                return Ok(None);
            }
            break;
        }
        let newline = bytes.iter().position(|byte| *byte == b'\n');
        let len = newline.unwrap_or(bytes.len());
        if capture {
            let kept = len.min(PREFIX_BYTES - prefix.len());
            prefix.extend_from_slice(&bytes[..kept]);
            discarded |= kept < len;
        }
        reader.consume(len + usize::from(newline.is_some()));
        consumed = true;
        if newline.is_some() {
            // Match str::lines: strip CR only as part of a CRLF terminator.
            if !discarded && prefix.last() == Some(&b'\r') {
                prefix.pop();
            }
            break;
        }
    }
    Ok(Some(prefix))
}

fn read_preview(
    mut reader: impl BufRead,
    start: usize,
    end: usize,
) -> io::Result<(String, usize, usize, bool)> {
    let mut output = BoundedText::new(MAX_TOOL_OUTPUT_BYTES);
    let mut total = 0;
    let mut shown = 0;
    let mut per_line_truncated = false;
    while let Some(prefix) = line_prefix(&mut reader, total + 1 >= start && total < end)? {
        total += 1;
        if total < start || total > end {
            continue;
        }
        let text = String::from_utf8_lossy(&prefix);
        per_line_truncated |= text.chars().count() > MAX_LINE_CHARS;
        if shown > 0 {
            output.push("\n");
        }
        output.push(&format!(
            "{total}. {}",
            truncate_line(&text, MAX_LINE_CHARS)
        ));
        shown += 1;
    }
    let (content, truncated) = output.finish();
    Ok((content, total, shown, truncated || per_line_truncated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn ranges_keep_line_numbers_counts_and_line_endings() {
        for (bytes, expected, total) in [
            (b"one\r\ntwo\r\nthree\n".as_slice(), "2. two", 3),
            (b"one\ntwo\r".as_slice(), "2. two\r", 2),
            (b"one\n\nthree".as_slice(), "2. ", 3),
        ] {
            let (output, actual_total, shown, truncated) =
                read_preview(Cursor::new(bytes), 2, 2).unwrap();
            assert_eq!(output, expected);
            assert_eq!(actual_total, total);
            assert_eq!(shown, 1);
            assert!(!truncated);
        }
        assert_eq!(
            read_preview(Cursor::new(b""), 1, usize::MAX).unwrap(),
            (String::new(), 0, 0, false)
        );
    }

    #[test]
    fn huge_single_lines_are_consumed_without_collecting_them() {
        let input = io::repeat(b'x')
            .take(32 * 1024 * 1024)
            .chain(Cursor::new(b"\nlast line\n"));
        let (output, total, shown, truncated) =
            read_preview(BufReader::with_capacity(1024, input), 1, 2).unwrap();
        assert_eq!((total, shown), (2, 2));
        assert!(truncated);
        assert!(output.contains("[...truncated]\n2. last line"));
        assert!(output.len() < MAX_LINE_CHARS + 100);
    }

    #[test]
    fn many_lines_keep_a_bounded_head_and_tail_preview() {
        let input = io::repeat(b'\n').take(100_000);
        let (output, total, shown, truncated) =
            read_preview(BufReader::new(input), 1, usize::MAX).unwrap();
        assert_eq!((total, shown), (100_000, 100_000));
        assert!(truncated);
        assert!(output.contains("1. \n2. "));
        assert!(output.ends_with("100000. "));
        assert!(output.len() < MAX_TOOL_OUTPUT_BYTES + 200);
    }

    #[tokio::test]
    async fn raw_output_is_lossy_and_invalid_ranges_do_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("output.log"),
            b"valid\nbad\xff\nend\xe4\xb8",
        )
        .unwrap();
        let tool = ReadFileTool::new(
            WorkspaceRoots::new(dir.path().to_path_buf(), vec![]),
            Arc::new(AgentReadState::new()),
        );
        let ctx = ToolContext::new(
            "read",
            Arc::new(crate::events::EventHub::new()),
            tokio_util::sync::CancellationToken::new(),
        );
        for (start, end, expected) in [(1, 1, "1. valid"), (2, 3, "2. bad�\n3. end�")] {
            let output = tool
                .execute(
                    serde_json::json!({"path":"output.log","start_line":start,"end_line":end}),
                    &ctx,
                )
                .await
                .unwrap();
            assert!(!output.is_error, "{}", output.content);
            assert!(output.content.ends_with(expected));
            assert_eq!(output.details.unwrap()["total_lines"], 3);
        }
        let output = tool
            .execute(
                serde_json::json!({"path":"output.log","start_line":3,"end_line":1}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(output.is_error);
        let output = tool
            .execute(
                serde_json::json!({"path":"output.log","start_line":10}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("beyond end"));
    }
}
