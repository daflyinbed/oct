use anyhow::{Context, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::read::validate_path_within;
use super::shared::{ReadCheck, ToolSharedState, file_mtime};
use super::write::validate_write_path;
use super::{AgentTool, ToolContext, ToolOutput};

/// Cap for the file line quoted in the nearest-match hint, in bytes.
const HINT_LINE_MAX_BYTES: usize = 200;

/// UI-only metadata for an edit_file call.
#[derive(Debug, Serialize)]
struct EditFileDetails {
    title: String,
    file_path: String,
    replacements: usize,
    replace_all: bool,
    /// True when the empty-old_string create-new-file path was taken.
    created: bool,
}

pub struct EditFileTool {
    working_dir: PathBuf,
    shared: Arc<ToolSharedState>,
}

impl EditFileTool {
    pub fn new(working_dir: PathBuf, shared: Arc<ToolSharedState>) -> Self {
        Self {
            working_dir,
            shared,
        }
    }

    /// Create-new-file path (empty `old_string`). The whole exists-check and
    /// write runs under the per-file lock so concurrent creates cannot race.
    async fn create_file(&self, args: &EditFileArgs) -> Result<ToolOutput> {
        let resolved = match validate_write_path(&self.working_dir, &args.file_path) {
            Ok(p) => p,
            Err(output) => return Ok(output),
        };

        if resolved.is_dir() {
            return Ok(ToolOutput::error(format!(
                "{} is a directory, not a file.",
                args.file_path
            )));
        }

        // Same conservative parent rule as write_file without create_dirs.
        if let Some(parent) = resolved.parent()
            && !parent.exists()
        {
            return Ok(ToolOutput::error(format!(
                "Parent directory does not exist: {}. Use write_file with create_dirs to create it.",
                parent.display()
            )));
        }

        let lock = self.shared.lock_for(&resolved);
        let _guard = lock.lock().await;

        // Creating over an empty existing file is allowed; over content it is not.
        if resolved.exists() {
            let is_non_empty = std::fs::metadata(&resolved)
                .map(|m| m.len() > 0)
                .unwrap_or(true);
            if is_non_empty {
                return Ok(ToolOutput::error(
                    "Cannot create new file - file already exists.",
                ));
            }
        }

        match tokio::fs::write(&resolved, &args.new_string).await {
            Ok(()) => {
                if let Some(mtime) = file_mtime(&resolved) {
                    self.shared.record_write(&resolved, mtime);
                }
                let details = EditFileDetails {
                    title: args.file_path.clone(),
                    file_path: args.file_path.clone(),
                    replacements: 0,
                    replace_all: false,
                    created: true,
                };
                Ok(ToolOutput::success_with_details(
                    format!("The file {} has been created successfully.", args.file_path),
                    details,
                ))
            }
            Err(e) => Ok(ToolOutput::error(format!("Failed to write file: {e}"))),
        }
    }

    /// Replace-match path for an existing file. Must be called while holding
    /// the per-file lock: the mtime check, the match, and the write form one
    /// read-modify-write critical section.
    async fn edit_existing(&self, args: &EditFileArgs, resolved: &Path) -> Result<ToolOutput> {
        match self.shared.check_read(resolved) {
            ReadCheck::NotRead => {
                return Ok(ToolOutput::error(
                    "File has not been read yet. Read it first before writing to it.",
                ));
            }
            ReadCheck::Stale => {
                return Ok(ToolOutput::error(
                    "File has been modified since read, either by the user or by a linter. \
                     Read it again before attempting to write it.",
                ));
            }
            ReadCheck::Fresh => {}
        }

        let content = match tokio::fs::read_to_string(resolved).await {
            Ok(c) => c,
            Err(e) => return Ok(ToolOutput::error(format!("Failed to read file: {e}"))),
        };

        // Detect CRLF once per file: match against an LF-normalized copy (the
        // model sends LF in JSON regardless of the file's line endings) and
        // convert back to CRLF before writing so line endings survive the edit.
        let has_crlf = content.contains("\r\n");
        let normalized = if has_crlf {
            content.replace("\r\n", "\n")
        } else {
            content
        };
        let old_string = if has_crlf {
            args.old_string.replace("\r\n", "\n")
        } else {
            args.old_string.clone()
        };
        let new_string = if has_crlf {
            args.new_string.replace("\r\n", "\n")
        } else {
            args.new_string.clone()
        };

        let matches: Vec<usize> = normalized
            .match_indices(old_string.as_str())
            .map(|(i, _)| i)
            .collect();

        if matches.is_empty() {
            let mut message = String::from(
                "The string to replace was not found in the file, use the read_file tool \
                 to see the correct string. The user may have changed the file since you \
                 last read it.",
            );
            if let Some(hint) = nearest_match_hint(&normalized, &old_string) {
                message.push_str(&hint);
            }
            return Ok(ToolOutput::error(message));
        }

        let replace_all = args.replace_all.unwrap_or(false);
        if matches.len() > 1 && !replace_all {
            return Ok(ToolOutput::error(format!(
                "The string to replace was found {} times in the file. Use replace_all to \
                 replace all occurrences, or include more context to only edit one occurrence.",
                matches.len()
            )));
        }

        let mut updated = if replace_all {
            normalized.replace(old_string.as_str(), new_string.as_str())
        } else {
            normalized.replacen(old_string.as_str(), new_string.as_str(), 1)
        };

        // `updated` contains no CRLF at this point (content and both strings
        // were normalized), so every LF can be safely turned back into CRLF.
        if has_crlf {
            updated = updated.replace('\n', "\r\n");
        }

        match tokio::fs::write(resolved, updated).await {
            Ok(()) => {
                // Record the post-write mtime so a follow-up edit in the same
                // run is not blocked as stale.
                if let Some(mtime) = file_mtime(resolved) {
                    self.shared.record_write(resolved, mtime);
                }
                let mut message =
                    format!("The file {} has been updated successfully.", args.file_path);
                if replace_all && matches.len() > 1 {
                    message.push_str(" All occurrences were successfully replaced.");
                }
                let details = EditFileDetails {
                    title: args.file_path.clone(),
                    file_path: args.file_path.clone(),
                    replacements: if replace_all { matches.len() } else { 1 },
                    replace_all,
                    created: false,
                };
                Ok(ToolOutput::success_with_details(message, details))
            }
            Err(e) => Ok(ToolOutput::error(format!("Failed to write file: {e}"))),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EditFileArgs {
    /// Path to the file to edit (relative to working directory or absolute)
    file_path: String,
    /// Text to replace. Empty only when creating a new file.
    old_string: String,
    /// Replacement text (must differ from old_string)
    new_string: String,
    /// Replace all occurrences. Default false.
    replace_all: Option<bool>,
}

/// Build the nearest-match hint for the not-found error: quote the first file
/// line containing the longest token of the first line of `old_string`.
/// Returns `None` when no usable token or matching line exists.
fn nearest_match_hint(content: &str, old_string: &str) -> Option<String> {
    let first_line = old_string.lines().next()?;
    let mut token: &str = "";
    for candidate in first_line.split_whitespace() {
        if candidate.len() > token.len() {
            token = candidate;
        }
    }
    if token.is_empty() {
        return None;
    }

    content
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains(token))
        .map(|(idx, line)| {
            format!(
                "\n\nNearest match: line {}: {}",
                idx + 1,
                cap_line_bytes(line, HINT_LINE_MAX_BYTES)
            )
        })
}

/// Cap a line to `max_bytes` of content on a UTF-8 char boundary, marking the
/// cut with an ellipsis character.
fn cap_line_bytes(line: &str, max_bytes: usize) -> String {
    if line.len() <= max_bytes {
        return line.to_string();
    }
    let mut end = max_bytes;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &line[..end])
}

#[async_trait]
impl AgentTool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Edit a file by exact-match string replacement: old_string must match the file \
         content exactly and be unique unless replace_all is true. The file must be read \
         with read_file before editing it. An empty old_string creates a new file with \
         new_string as its content."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(EditFileArgs)).unwrap()
    }

    fn title(&self, args: &serde_json::Value) -> String {
        args.get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("edit_file")
            .to_string()
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let args: EditFileArgs =
            serde_json::from_value(args).context("Invalid arguments for edit_file")?;

        // Reject no-op edits before touching the filesystem.
        if args.old_string == args.new_string {
            return Ok(ToolOutput::error("Old string and new string are the same"));
        }

        // Empty old_string means "create a new file".
        if args.old_string.is_empty() {
            return self.create_file(&args).await;
        }

        let resolved = match validate_path_within(&self.working_dir, &args.file_path) {
            Ok(p) => p,
            Err(output) => return Ok(output),
        };

        if resolved.is_dir() {
            return Ok(ToolOutput::error(format!(
                "{} is a directory, not a file.",
                args.file_path
            )));
        }

        // Serialize read-modify-write on the same file: concurrent tool calls
        // from the agent loop cannot interleave the mtime check with the write.
        let lock = self.shared.lock_for(&resolved);
        let _guard = lock.lock().await;
        self.edit_existing(&args, &resolved).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::read::ReadFileTool;
    use crate::tools::write::WriteFileTool;
    use serde_json::json;

    /// A ToolContext wired to a throwaway event hub; edit_file does
    /// not stream, so the context is only needed for the trait signature.
    fn noop_ctx() -> ToolContext {
        ToolContext::new("test", std::sync::Arc::new(crate::agent::EventHub::new()))
    }

    /// A temp working directory with the edit/read/write tools sharing one
    /// `ToolSharedState`, mirroring how `default_tools` wires them up.
    struct Fixture {
        dir: PathBuf,
        edit: EditFileTool,
        read: ReadFileTool,
        write: WriteFileTool,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "oct-edit-test-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let shared = Arc::new(ToolSharedState::new());
            Self {
                edit: EditFileTool::new(dir.clone(), shared.clone()),
                read: ReadFileTool::new(dir.clone(), shared.clone()),
                write: WriteFileTool::new(dir.clone(), shared),
                dir,
            }
        }

        fn file(&self, name: &str) -> PathBuf {
            self.dir.join(name)
        }

        async fn read(&self, name: &str) {
            let out = self
                .read
                .execute(json!({ "path": name }), &noop_ctx())
                .await
                .unwrap();
            assert!(!out.is_error, "fixture read failed: {}", out.content);
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[tokio::test]
    async fn single_replacement_succeeds() {
        let fx = Fixture::new("single");
        std::fs::write(fx.file("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        fx.read("a.txt").await;

        let out = fx
            .edit
            .execute(json!({
                "file_path": "a.txt",
                "old_string": "beta",
                "new_string": "BETA"
            }), &noop_ctx())
            .await
            .unwrap();

        assert!(!out.is_error);
        assert_eq!(out.content, "The file a.txt has been updated successfully.");
        assert_eq!(
            std::fs::read_to_string(fx.file("a.txt")).unwrap(),
            "alpha\nBETA\ngamma\n"
        );
    }

    #[tokio::test]
    async fn old_and_new_identical_is_rejected() {
        let fx = Fixture::new("same");
        std::fs::write(fx.file("a.txt"), "content\n").unwrap();
        fx.read("a.txt").await;

        let out = fx
            .edit
            .execute(json!({
                "file_path": "a.txt",
                "old_string": "content",
                "new_string": "content"
            }), &noop_ctx())
            .await
            .unwrap();

        assert!(out.is_error);
        assert_eq!(out.content, "Old string and new string are the same");
    }

    #[tokio::test]
    async fn not_found_error_includes_guidance_and_nearest_match_hint() {
        let fx = Fixture::new("notfound");
        std::fs::write(
            fx.file("a.txt"),
            "mod tests;\nfn main() {\n    println!(\"hi\");\n}\n",
        )
        .unwrap();
        fx.read("a.txt").await;

        let out = fx
            .edit
            .execute(json!({
                "file_path": "a.txt",
                "old_string": "fn main() {}\n",
                "new_string": "fn main() { return }\n"
            }), &noop_ctx())
            .await
            .unwrap();

        assert!(out.is_error);
        assert!(
            out.content
                .contains("use the read_file tool to see the correct string")
        );
        assert!(
            out.content
                .contains("The user may have changed the file since you last read it.")
        );
        // Longest token of "fn main() {}" is "main()"; first matching line is line 2.
        assert!(
            out.content
                .contains("\n\nNearest match: line 2: fn main() {")
        );
    }

    #[tokio::test]
    async fn not_found_without_any_matching_token_omits_hint() {
        let fx = Fixture::new("nohint");
        std::fs::write(fx.file("a.txt"), "one two three\n").unwrap();
        fx.read("a.txt").await;

        let out = fx
            .edit
            .execute(json!({
                "file_path": "a.txt",
                "old_string": "zebra",
                "new_string": "horse"
            }), &noop_ctx())
            .await
            .unwrap();

        assert!(out.is_error);
        assert!(out.content.contains("The string to replace was not found"));
        assert!(!out.content.contains("Nearest match"));
    }

    #[tokio::test]
    async fn multiple_matches_need_replace_all() {
        let fx = Fixture::new("multi");
        std::fs::write(fx.file("a.txt"), "x = 1\ny = 1\nz = 2\n").unwrap();
        fx.read("a.txt").await;

        let out = fx
            .edit
            .execute(json!({
                "file_path": "a.txt",
                "old_string": "= 1",
                "new_string": "= 10"
            }), &noop_ctx())
            .await
            .unwrap();

        assert!(out.is_error);
        assert_eq!(
            out.content,
            "The string to replace was found 2 times in the file. Use replace_all to replace \
             all occurrences, or include more context to only edit one occurrence."
        );

        // With replace_all both occurrences are replaced and the message says so.
        let out = fx
            .edit
            .execute(json!({
                "file_path": "a.txt",
                "old_string": "= 1",
                "new_string": "= 10",
                "replace_all": true
            }), &noop_ctx())
            .await
            .unwrap();

        assert!(!out.is_error);
        assert_eq!(
            out.content,
            "The file a.txt has been updated successfully. All occurrences were successfully replaced."
        );
        assert_eq!(
            std::fs::read_to_string(fx.file("a.txt")).unwrap(),
            "x = 10\ny = 10\nz = 2\n"
        );
    }

    #[tokio::test]
    async fn replace_all_with_single_occurrence_has_no_suffix() {
        let fx = Fixture::new("all-single");
        std::fs::write(fx.file("a.txt"), "only one here\n").unwrap();
        fx.read("a.txt").await;

        let out = fx
            .edit
            .execute(json!({
                "file_path": "a.txt",
                "old_string": "one",
                "new_string": "1",
                "replace_all": true
            }), &noop_ctx())
            .await
            .unwrap();

        assert!(!out.is_error);
        assert_eq!(out.content, "The file a.txt has been updated successfully.");
    }

    #[tokio::test]
    async fn empty_old_string_creates_a_new_file() {
        let fx = Fixture::new("create");

        let out = fx
            .edit
            .execute(json!({
                "file_path": "new.txt",
                "old_string": "",
                "new_string": "brand new\n"
            }), &noop_ctx())
            .await
            .unwrap();

        assert!(!out.is_error);
        assert_eq!(
            out.content,
            "The file new.txt has been created successfully."
        );
        assert_eq!(
            std::fs::read_to_string(fx.file("new.txt")).unwrap(),
            "brand new\n"
        );

        // Creating over an existing NON-empty file is an error.
        let out = fx
            .edit
            .execute(json!({
                "file_path": "new.txt",
                "old_string": "",
                "new_string": "other\n"
            }), &noop_ctx())
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.content, "Cannot create new file - file already exists.");

        // Creating over an existing EMPTY file is allowed.
        std::fs::write(fx.file("empty.txt"), "").unwrap();
        let out = fx
            .edit
            .execute(json!({
                "file_path": "empty.txt",
                "old_string": "",
                "new_string": "filled\n"
            }), &noop_ctx())
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("has been created successfully"));
        assert_eq!(
            std::fs::read_to_string(fx.file("empty.txt")).unwrap(),
            "filled\n"
        );
    }

    #[tokio::test]
    async fn missing_parent_dir_for_create_is_an_error() {
        let fx = Fixture::new("noparent");

        let out = fx
            .edit
            .execute(json!({
                "file_path": "missing_dir/new.txt",
                "old_string": "",
                "new_string": "content\n"
            }), &noop_ctx())
            .await
            .unwrap();

        assert!(out.is_error);
        assert!(out.content.starts_with("Parent directory does not exist: "));
    }

    #[tokio::test]
    async fn crlf_file_keeps_crlf_line_endings() {
        let fx = Fixture::new("crlf");
        std::fs::write(fx.file("a.txt"), b"line1\r\nline2\r\nline3\r\n").unwrap();
        fx.read("a.txt").await;

        let out = fx
            .edit
            .execute(json!({
                "file_path": "a.txt",
                "old_string": "line2",
                "new_string": "LINE2"
            }), &noop_ctx())
            .await
            .unwrap();

        assert!(!out.is_error);
        assert_eq!(
            std::fs::read(fx.file("a.txt")).unwrap(),
            b"line1\r\nLINE2\r\nline3\r\n".to_vec()
        );
    }

    #[tokio::test]
    async fn edit_requires_a_prior_read() {
        let fx = Fixture::new("mustread");
        std::fs::write(fx.file("a.txt"), "hello world\n").unwrap();

        let out = fx
            .edit
            .execute(json!({
                "file_path": "a.txt",
                "old_string": "hello",
                "new_string": "goodbye"
            }), &noop_ctx())
            .await
            .unwrap();

        assert!(out.is_error);
        assert_eq!(
            out.content,
            "File has not been read yet. Read it first before writing to it."
        );

        // After a read (any range), the same edit succeeds.
        fx.read("a.txt").await;
        let out = fx
            .edit
            .execute(json!({
                "file_path": "a.txt",
                "old_string": "hello",
                "new_string": "goodbye"
            }), &noop_ctx())
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(
            std::fs::read_to_string(fx.file("a.txt")).unwrap(),
            "goodbye world\n"
        );
    }

    #[tokio::test]
    async fn write_then_edit_succeeds_without_read() {
        let fx = Fixture::new("writethen");

        let out = fx
            .write
            .execute(json!({
                "path": "a.txt",
                "content": "count = 1\n"
            }), &noop_ctx())
            .await
            .unwrap();
        assert!(!out.is_error);

        // write_file recorded the fresh mtime, so no read is required first.
        let out = fx
            .edit
            .execute(json!({
                "file_path": "a.txt",
                "old_string": "count = 1",
                "new_string": "count = 2"
            }), &noop_ctx())
            .await
            .unwrap();

        assert!(!out.is_error);
        assert_eq!(
            std::fs::read_to_string(fx.file("a.txt")).unwrap(),
            "count = 2\n"
        );
    }

    #[tokio::test]
    async fn externally_modified_file_is_reported_stale() {
        let fx = Fixture::new("stale");
        std::fs::write(fx.file("a.txt"), "original\n").unwrap();
        fx.read("a.txt").await;

        // Simulate an external modification (user/linter) by changing the
        // file's mtime after our read; a clearly different timestamp avoids
        // any dependence on filesystem mtime granularity.
        let file = std::fs::File::options()
            .write(true)
            .open(fx.file("a.txt"))
            .unwrap();
        file.set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(3600)),
        )
        .unwrap();
        drop(file);

        let out = fx
            .edit
            .execute(json!({
                "file_path": "a.txt",
                "old_string": "original",
                "new_string": "edited"
            }), &noop_ctx())
            .await
            .unwrap();

        assert!(out.is_error);
        assert_eq!(
            out.content,
            "File has been modified since read, either by the user or by a linter. \
             Read it again before attempting to write it."
        );
    }

    #[tokio::test]
    async fn directory_target_is_rejected() {
        let fx = Fixture::new("isdir");
        std::fs::create_dir_all(fx.file("subdir")).unwrap();

        let out = fx
            .edit
            .execute(json!({
                "file_path": "subdir",
                "old_string": "a",
                "new_string": "b"
            }), &noop_ctx())
            .await
            .unwrap();

        assert!(out.is_error);
        assert_eq!(out.content, "subdir is a directory, not a file.");
    }

    #[tokio::test]
    async fn path_outside_working_dir_is_denied() {
        let fx = Fixture::new("escape");
        // A real file in the parent of the working dir, so canonicalization
        // succeeds and the sandbox check (not the existence check) rejects it.
        let outside = std::env::temp_dir().join(format!(
            "oct-edit-test-outside-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&outside, "secret\n").unwrap();

        let out = fx
            .edit
            .execute(json!({
                "file_path": format!("../{}", outside.file_name().unwrap().to_string_lossy()),
                "old_string": "secret",
                "new_string": "b"
            }), &noop_ctx())
            .await
            .unwrap();

        assert!(out.is_error);
        assert!(out.content.contains("outside the working directory"));
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "secret\n");
        let _ = std::fs::remove_file(&outside);
    }

    #[tokio::test]
    async fn not_found_on_multibyte_line_cuts_hint_on_char_boundary() {
        let fx = Fixture::new("hintcap");
        // 150 CJK chars (450 bytes) + " token" makes the matching line far
        // longer than the 200-byte hint cap.
        let long_line = format!("{} token", "中".repeat(150));
        std::fs::write(fx.file("a.txt"), format!("intro\n{long_line}\n")).unwrap();
        fx.read("a.txt").await;

        let out = fx
            .edit
            .execute(json!({
                "file_path": "a.txt",
                // Longest token of the first line is "token", which only
                // appears on the over-long line 2.
                "old_string": "token\nrest of the missing text",
                "new_string": "x"
            }), &noop_ctx())
            .await
            .unwrap();

        assert!(out.is_error);
        assert!(out.content.contains("\n\nNearest match: line 2: \u{4e2d}"));
        // Capped at a char boundary and marked with an ellipsis, never mid-char.
        assert!(out.content.ends_with('…'));
        assert!(!out.content.contains('\u{FFFD}'));
    }

    #[test]
    fn cap_line_bytes_marks_and_respects_boundaries() {
        // Under the cap: unchanged.
        assert_eq!(cap_line_bytes("short", HINT_LINE_MAX_BYTES), "short");

        // Multibyte cut floors to a char boundary (200 -> 198 bytes) and adds ….
        let long_line = format!("{}end", "中".repeat(150));
        let capped = cap_line_bytes(&long_line, HINT_LINE_MAX_BYTES);
        assert_eq!(capped.len(), 198 + "…".len());
        assert!(capped.ends_with('…'));
        assert!(capped.starts_with(&"中".repeat(66)));
    }
}
