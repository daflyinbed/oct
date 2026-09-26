//! Action extraction (ADR 0004 §3): `(tool_name, args)` → the neutral
//! [`PermissionAction`] the decision table consumes. The single place that
//! knows which argument of which tool carries the path/command, so rule
//! matching and approval display always describe the same action the model
//! asked for.
//!
//! Failure policy (§1.1's "the gate never steals the tool's error
//! semantics"): a call whose arguments cannot produce a descriptor — missing
//! parameter, non-string value, path that fails to resolve — maps to
//! [`PermissionAction::Trusted`] and proceeds. The TOOL then reports the
//! error itself ("Path not found: …"), which is both the message the model
//! can act on and the one existing tests pin; the gate must not turn a
//! typo'd path into a permission question (or a silent denial) before the
//! tool ever sees it.

use serde_json::Value;
use std::path::PathBuf;

use crate::permissions::evaluator::PermissionAction;
use crate::tools::workspace::WorkspaceRoots;

/// Extract the action a tool call is about to perform.
pub fn describe(tool_name: &str, args: &Value, roots: &WorkspaceRoots) -> PermissionAction {
    match tool_name {
        // The read class shares one decision row (§2): all four tools
        // resolve their optional path argument the same way.
        "read_file" | "list_dir" | "grep" | "glob" => read_action(args, roots),
        "write_file" => write_action(args, roots, "path"),
        // edit_file names its path argument `file_path` (matching the
        // editor-tool convention); it resolves like a write target because
        // edits may create the file (empty `old_string`).
        "edit_file" => write_action(args, roots, "file_path"),
        "execute_command" => command_action(args),
        // MCP tools by their fully qualified name — pattern semantics for
        // rules are the exact name or the `mcp__{server}__*` wildcard.
        name if name.starts_with("mcp__") => PermissionAction::Mcp {
            tool: name.to_string(),
        },
        // task / task_output / task_stop (the free-pass class) and anything
        // unknown (the loop's "Unknown tool" error is that call's outcome).
        _ => PermissionAction::Trusted,
    }
}

/// Read-class extraction: resolve the optional `path` argument. An ABSENT
/// path runs against the primary root — inside the workspace by definition
/// (`outside = false`), matching the tools' own defaulting — while a
/// present-but-invalid value stays Trusted so the tool reports the argument
/// error itself.
fn read_action(args: &Value, roots: &WorkspaceRoots) -> PermissionAction {
    match args.get("path") {
        None => PermissionAction::Read {
            path: roots
                .primary()
                .canonicalize()
                .unwrap_or_else(|_| roots.primary().to_path_buf()),
            outside: false,
        },
        Some(Value::String(path)) => match roots.resolve_path(path) {
            // The resolver canonicalizes (symlinks followed) and classifies
            // containment; both feed the decision table and the approval
            // card. A failed resolve (missing path) is the TOOL's error.
            Ok(resolved) => PermissionAction::Read {
                path: resolved.path,
                outside: resolved.outside,
            },
            Err(_) => PermissionAction::Trusted,
        },
        Some(_) => PermissionAction::Trusted,
    }
}

/// Write/edit extraction over the tool's path argument, resolved through
/// the write resolver (deepest existing ancestor canonicalized, so what the
/// user approves on the card is the symlink's real destination).
fn write_action(args: &Value, roots: &WorkspaceRoots, key: &str) -> PermissionAction {
    let class = |path: PathBuf, outside: bool| {
        if key == "path" {
            PermissionAction::Write { path, outside }
        } else {
            PermissionAction::Edit { path, outside }
        }
    };
    match str_arg(args, key) {
        Some(path) => match roots.resolve_write_path(path) {
            Ok(resolved) => class(resolved.path, resolved.outside),
            Err(_) => PermissionAction::Trusted,
        },
        None => PermissionAction::Trusted,
    }
}

/// Command extraction: the raw command string (prefix matching happens in
/// the rule layer). `working_dir` is deliberately NOT part of the action —
/// §3 keeps Ask independent of the cwd; the resolved working dir is display
/// context the gate could surface later, not a decision input.
fn command_action(args: &Value) -> PermissionAction {
    match str_arg(args, "command") {
        Some(command) => PermissionAction::Command {
            command: command.to_string(),
        },
        None => PermissionAction::Trusted,
    }
}

/// A string-typed argument, `None` for missing or non-string values (the
/// tool's own argument deserialization reports those).
fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        primary: tempfile::TempDir,
        additional: tempfile::TempDir,
        outside: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                primary: tempfile::tempdir().unwrap(),
                additional: tempfile::tempdir().unwrap(),
                outside: tempfile::tempdir().unwrap(),
            }
        }

        fn roots(&self) -> WorkspaceRoots {
            WorkspaceRoots::new(
                self.primary.path().to_path_buf(),
                vec![self.additional.path().to_path_buf()],
            )
        }
    }

    fn read(action: &PermissionAction) -> (&PathBuf, &bool) {
        match action {
            PermissionAction::Read { path, outside } => (path, outside),
            other => panic!("expected Read, got {other:?}"),
        }
    }

    fn written(action: &PermissionAction) -> (&PathBuf, &bool) {
        match action {
            PermissionAction::Write { path, outside } => (path, outside),
            other => panic!("expected Write, got {other:?}"),
        }
    }

    #[test]
    fn read_class_resolves_relative_and_absolute_requests() {
        let fx = Fixture::new();
        std::fs::write(fx.primary.path().join("a.txt"), "x").unwrap();
        let extra = fx.additional.path().join("notes.md");
        std::fs::write(&extra, "x").unwrap();
        let outside_file = fx.outside.path().join("secret");
        std::fs::write(&outside_file, "x").unwrap();

        let roots = fx.roots();
        for tool in ["read_file", "list_dir", "grep", "glob"] {
            // The action must outlive the borrowed fields below.
            // Relative → primary root, inside.
            let action = describe(
                tool,
                &serde_json::json!({"path": "a.txt", "pattern": "x"}),
                &roots,
            );
            let (path, outside) = read(&action);
            assert_eq!(*path, fx.primary.path().canonicalize().unwrap().join("a.txt"));
            assert!(!*outside, "{tool}");

            // Absolute inside an additional root.
            let action = describe(
                tool,
                &serde_json::json!({"path": extra.display().to_string()}),
                &roots,
            );
            let (path, outside) = read(&action);
            assert_eq!(*path, extra.canonicalize().unwrap());
            assert!(!*outside, "{tool}");

            // Absolute outside every root: resolved AND classified.
            let action = describe(
                tool,
                &serde_json::json!({"path": outside_file.display().to_string()}),
                &roots,
            );
            let (path, outside) = read(&action);
            assert_eq!(*path, outside_file.canonicalize().unwrap());
            assert!(*outside, "{tool}");
        }

        // No path argument: the primary root itself, never outside.
        let action = describe("list_dir", &serde_json::json!({}), &roots);
        let (path, outside) = read(&action);
        assert_eq!(*path, fx.primary.path().canonicalize().unwrap());
        assert!(!outside);
        // read_file without a path takes the same default (its schema
        // requires the argument; the descriptor does not special-case it).
        let action = describe("read_file", &serde_json::json!({}), &roots);
        let (path, _) = read(&action);
        assert_eq!(*path, fx.primary.path().canonicalize().unwrap());
    }

    #[test]
    fn file_writes_resolve_new_targets_and_classify() {
        let fx = Fixture::new();
        let roots = fx.roots();

        // A not-yet-existing file inside the workspace: the ancestor walk
        // anchors it under the primary root.
        let action = describe(
            "write_file",
            &serde_json::json!({"path": "new-dir/f.txt", "content": "x"}),
            &roots,
        );
        let (path, outside) = written(&action);
        assert_eq!(
            *path,
            fx.primary.path().canonicalize().unwrap().join("new-dir/f.txt")
        );
        assert!(!outside);

        // An outside target resolves to the real absolute destination.
        let target = fx.outside.path().join("cfg.conf");
        let action = describe(
            "write_file",
            &serde_json::json!({"path": target.display().to_string(), "content": "x"}),
            &roots,
        );
        let (path, outside) = written(&action);
        assert_eq!(
            *path,
            fx.outside.path().canonicalize().unwrap().join("cfg.conf")
        );
        assert!(outside);

        // edit_file reads the `file_path` argument and lands in the Edit class.
        assert!(matches!(
            describe(
                "edit_file",
                &serde_json::json!({"file_path": "f.txt", "old_string": "", "new_string": "x"}),
                &roots,
            ),
            PermissionAction::Edit { outside: false, .. }
        ));
        // ...while write_file reads `path`.
        assert!(matches!(
            describe("write_file", &serde_json::json!({"path": "f.txt"}), &roots),
            PermissionAction::Write { .. }
        ));
    }

    #[test]
    fn commands_carry_the_raw_command_string() {
        let fx = Fixture::new();
        let roots = fx.roots();
        assert_eq!(
            describe(
                "execute_command",
                &serde_json::json!({"command": "cargo build --release", "run_in_background": true}),
                &roots,
            ),
            PermissionAction::Command {
                command: "cargo build --release".into()
            }
        );
        // The working dir is not part of the action (§3).
        assert_eq!(
            describe(
                "execute_command",
                &serde_json::json!({"command": "ls", "working_dir": "sub"}),
                &roots,
            ),
            PermissionAction::Command { command: "ls".into() }
        );
    }

    #[test]
    fn mcp_tools_map_by_qualified_name() {
        let fx = Fixture::new();
        assert_eq!(
            describe("mcp__github__create_issue", &serde_json::json!({}), &fx.roots()),
            PermissionAction::Mcp {
                tool: "mcp__github__create_issue".into()
            }
        );
    }

    #[test]
    fn unresolvable_calls_stay_trusted_so_the_tool_errors() {
        let fx = Fixture::new();
        let roots = fx.roots();

        // Missing path (read resolution fails): Trusted, the read tool
        // itself reports "Path not found".
        assert_eq!(
            describe("read_file", &serde_json::json!({"path": "no-such.txt"}), &roots),
            PermissionAction::Trusted
        );
        // Missing parameters entirely.
        assert_eq!(
            describe("write_file", &serde_json::json!({}), &roots),
            PermissionAction::Trusted
        );
        assert_eq!(
            describe("edit_file", &serde_json::json!({"path": "wrong-key-name"}), &roots),
            PermissionAction::Trusted
        );
        assert_eq!(
            describe("execute_command", &serde_json::json!({"timeout_secs": 5}), &roots),
            PermissionAction::Trusted
        );
        // Non-string argument values.
        assert_eq!(
            describe("read_file", &serde_json::json!({"path": 42}), &roots),
            PermissionAction::Trusted
        );

        // The free-pass class and unknown tools.
        for name in ["task", "task_output", "task_stop", "no_such_tool"] {
            assert_eq!(
                describe(name, &serde_json::json!({"a": 1}), &roots),
                PermissionAction::Trusted
            );
        }
    }
}
