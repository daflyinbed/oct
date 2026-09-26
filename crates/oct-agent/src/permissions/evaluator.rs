//! The decision core (ADR 0004 §2): action + mode + rules → Allow | Ask |
//! Deny, with fixed precedence deny > ask > allow > mode default.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::permissions::rules::{Effect, RuleSet};

/// Per-conversation tool permission mode (`conversations.permission_mode`,
/// §1.5): the default decision when no rule matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Write/execute/outside-read all ask.
    Default,
    /// In-workspace writes auto-allowed; everything else still asks.
    AcceptEdits,
    /// Everything allowed — deny rules (and the built-in exec-root deny)
    /// still win.
    Bypass,
}

impl PermissionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            PermissionMode::Default => "default",
            PermissionMode::AcceptEdits => "accept_edits",
            PermissionMode::Bypass => "bypass",
        }
    }

    /// Parse the DB/API string form; `None` for anything else. The column
    /// is CHECK-constrained, so the `None` arm is defense in depth.
    pub fn from_str_opt(value: &str) -> Option<Self> {
        match value {
            "default" => Some(PermissionMode::Default),
            "accept_edits" => Some(PermissionMode::AcceptEdits),
            "bypass" => Some(PermissionMode::Bypass),
            _ => None,
        }
    }
}

/// Neutral description of what a tool call is about to do (§3) — the
/// single input of the decision table, shared by rule matching and the
/// approval display. The gate (a later PR) extracts it at the loop's
/// dispatch point from `(tool_name, args)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionAction {
    /// The read class: `read_file` / `list_dir` / `grep` / `glob`.
    /// `path` is the RESOLVED path (canonicalized, symlink targets
    /// followed); `outside` is the workspace containment verdict.
    Read { path: PathBuf, outside: bool },
    Write { path: PathBuf, outside: bool },
    Edit { path: PathBuf, outside: bool },
    /// `execute_command` (foreground and `run_in_background` alike — §1.7).
    Command { command: String },
    /// An MCP tool invocation, named by its fully qualified name.
    Mcp { tool: String },
    /// Free-pass tools (`task`/`task_output`/`task_stop`, §2): constant
    /// Allow; no descriptor is built for them at the gate.
    Trusted,
}

impl PermissionAction {
    /// The action's display class (`read`/`write`/`edit`/`command`/`mcp`/
    /// `trusted`) — the approval card's kind badge.
    pub fn kind_str(&self) -> &'static str {
        match self {
            PermissionAction::Read { .. } => "read",
            PermissionAction::Write { .. } => "write",
            PermissionAction::Edit { .. } => "edit",
            PermissionAction::Command { .. } => "command",
            PermissionAction::Mcp { .. } => "mcp",
            PermissionAction::Trusted => "trusted",
        }
    }

    /// The resolved path of a file-class action (read/write/edit), `None`
    /// for every other class.
    pub fn file_path(&self) -> Option<&Path> {
        match self {
            PermissionAction::Read { path, .. }
            | PermissionAction::Write { path, .. }
            | PermissionAction::Edit { path, .. } => Some(path),
            PermissionAction::Command { .. }
            | PermissionAction::Mcp { .. }
            | PermissionAction::Trusted => None,
        }
    }

    /// One-line display summary for approval cards and events: the real
    /// path (post-symlink) for file actions, the command for execute, the
    /// qualified name for MCP.
    pub fn title(&self) -> String {
        match self {
            PermissionAction::Read { path, .. }
            | PermissionAction::Write { path, .. }
            | PermissionAction::Edit { path, .. } => path.display().to_string(),
            PermissionAction::Command { command } => command.clone(),
            PermissionAction::Mcp { tool } => tool.clone(),
            PermissionAction::Trusted => String::new(),
        }
    }
}

/// The gate's verdict (§1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Execute the tool.
    Allow,
    /// Park on an approval (registry + oneshot; the gate owns waiting).
    Ask,
    /// Refuse with a model-visible error ToolOutput.
    Deny,
}

/// The gate (§1.1): rule chain first (fixed precedence deny > ask > allow,
/// same effect wins on any single hit), mode default last. Two denies sit
/// above every rule: the built-in exec-root deny and — trivially — the
/// free-pass class, which no rule can name.
pub fn decide(action: &PermissionAction, mode: PermissionMode, rules: &RuleSet) -> Decision {
    // Free-pass tools (§2's constant-Allow row): no descriptor is built for
    // them, so no rule ever matches — deny included.
    if matches!(action, PermissionAction::Trusted) {
        return Decision::Allow;
    }

    // Built-in exec-root deny (§1.3): outranks every rule and every mode —
    // the background-task output stays readable through `task_output` only.
    if rules.denies_exec_root(action) {
        return Decision::Deny;
    }

    // The rule chain. A deny returns on the spot; ask and allow are
    // remembered so ask beats allow regardless of rule order (§1.3: no
    // specificity ranking).
    let mut saw_ask = false;
    let mut saw_allow = false;
    for rule in rules.rules() {
        if !rule.matches(action) {
            continue;
        }
        match rule.effect {
            Effect::Deny => return Decision::Deny,
            Effect::Ask => saw_ask = true,
            Effect::Allow => saw_allow = true,
        }
    }
    if saw_ask {
        return Decision::Ask;
    }
    if saw_allow {
        return Decision::Allow;
    }
    mode_default(action, mode)
}

/// The §2 decision table's mode axis — the verdict when no rule matched.
fn mode_default(action: &PermissionAction, mode: PermissionMode) -> Decision {
    if mode == PermissionMode::Bypass {
        // Bypass allows every class; deny rules already returned earlier.
        return Decision::Allow;
    }
    match action {
        // Inside reads are constant-Allow in every mode. Outside reads ask
        // even under accept_edits: that mode only relaxes in-workspace
        // writes.
        PermissionAction::Read { outside, .. } => {
            if *outside {
                Decision::Ask
            } else {
                Decision::Allow
            }
        }
        // accept_edits auto-allows only IN-workspace writes; outside writes
        // keep asking (the approval card carries the outside mark).
        PermissionAction::Write { outside, .. } | PermissionAction::Edit { outside, .. } => {
            if !*outside && mode == PermissionMode::AcceptEdits {
                Decision::Allow
            } else {
                Decision::Ask
            }
        }
        PermissionAction::Command { .. } | PermissionAction::Mcp { .. } => Decision::Ask,
        PermissionAction::Trusted => Decision::Allow,
    }
}

/// A conservative `pattern` for an "always allow" rule off this action
/// (§3), offered with the approval so the user can edit before submitting:
/// `execute_command` → the first two whitespace tokens of the command
/// (`cargo build` for `cargo build --release`); file actions → the file's
/// immediate parent directory as an absolute `{dir}/**` glob. Everything
/// else has no meaningful pattern.
pub fn rule_suggestion(action: &PermissionAction) -> Option<String> {
    match action {
        PermissionAction::Command { command } => {
            let prefix = command
                .split_whitespace()
                .take(2)
                .collect::<Vec<_>>()
                .join(" ");
            // An empty command must not become an empty pattern: an empty
            // prefix would match every command.
            (!prefix.is_empty()).then_some(prefix)
        }
        PermissionAction::Read { path, .. }
        | PermissionAction::Write { path, .. }
        | PermissionAction::Edit { path, .. } => {
            let dir = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(path);
            Some(format!("{}/**", dir.display()))
        }
        PermissionAction::Mcp { .. } | PermissionAction::Trusted => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::rules::{Effect, Rule};

    /// A RuleSet whose exec root points somewhere the test paths never
    /// live, so the built-in deny stays out of the table's way (it has its
    /// own tests below).
    fn plain_ruleset() -> RuleSet {
        RuleSet::new("/does-not-exist-exec")
    }

    fn ruleset_with(rule: Rule) -> RuleSet {
        let mut set = plain_ruleset();
        set.add(rule);
        set
    }

    fn read(path: &str, outside: bool) -> PermissionAction {
        PermissionAction::Read {
            path: PathBuf::from(path),
            outside,
        }
    }

    fn write(path: &str, outside: bool) -> PermissionAction {
        PermissionAction::Write {
            path: PathBuf::from(path),
            outside,
        }
    }

    fn edit(path: &str, outside: bool) -> PermissionAction {
        PermissionAction::Edit {
            path: PathBuf::from(path),
            outside,
        }
    }

    fn command() -> PermissionAction {
        PermissionAction::Command {
            command: "cargo build --release".into(),
        }
    }

    fn mcp() -> PermissionAction {
        PermissionAction::Mcp {
            tool: "mcp__github__create_issue".into(),
        }
    }

    /// A rule that matches the given action (per-class tool name, NULL
    /// pattern).
    fn matching_rule(action: &PermissionAction, effect: Effect) -> Rule {
        let tool = match action {
            PermissionAction::Read { .. } => "read_file",
            PermissionAction::Write { .. } => "write_file",
            PermissionAction::Edit { .. } => "edit_file",
            PermissionAction::Command { .. } => "execute_command",
            PermissionAction::Mcp { tool } => return Rule {
                effect,
                tool: tool.clone(),
                pattern: None,
            },
            PermissionAction::Trusted => "task",
        };
        Rule {
            effect,
            tool: tool.to_string(),
            pattern: None,
        }
    }

    /// The §2 table, cell by cell, with NO rules: every mode × every
    /// action × inside/outside. The tuples carry (default, accept_edits,
    /// bypass) straight from the ADR.
    #[test]
    fn decision_table_matches_adr_section_2() {
        let cases: Vec<(PermissionAction, [Decision; 3])> = vec![
            // read/listdir/grep/glob — inside: constant Allow.
            (read("/ws/src/a.rs", false), [Decision::Allow, Decision::Allow, Decision::Allow]),
            // read class — outside: Ask, Ask, Allow.
            (read("/etc/hosts", true), [Decision::Ask, Decision::Ask, Decision::Allow]),
            // write/edit — inside: Ask, Allow, Allow.
            (write("/ws/src/a.rs", false), [Decision::Ask, Decision::Allow, Decision::Allow]),
            (edit("/ws/src/a.rs", false), [Decision::Ask, Decision::Allow, Decision::Allow]),
            // write/edit — outside: Ask in both non-bypass modes.
            (write("/etc/app.conf", true), [Decision::Ask, Decision::Ask, Decision::Allow]),
            (edit("/etc/app.conf", true), [Decision::Ask, Decision::Ask, Decision::Allow]),
            // execute_command (incl. background): Ask, Ask, Allow.
            (command(), [Decision::Ask, Decision::Ask, Decision::Allow]),
            // mcp tools: Ask, Ask, Allow.
            (mcp(), [Decision::Ask, Decision::Ask, Decision::Allow]),
            // task/task_output/task_stop: constant Allow.
            (PermissionAction::Trusted, [Decision::Allow, Decision::Allow, Decision::Allow]),
        ];
        let modes = [
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::Bypass,
        ];
        let rules = plain_ruleset();
        for (action, expected) in &cases {
            for (mode, expected) in modes.iter().zip(expected.iter()) {
                assert_eq!(
                    &decide(action, *mode, &rules),
                    expected,
                    "action={action:?} mode={mode:?}"
                );
            }
        }
    }

    /// Rule precedence over the mode default, exhaustively: for every
    /// non-Trusted action × every mode × {allow, ask, deny} rule — deny
    /// wins everywhere (bypass included), ask beats allow, allow beats the
    /// mode default. Trusted ignores rules entirely.
    #[test]
    fn rule_precedence_overrides_every_mode_cell() {
        let modes = [
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::Bypass,
        ];
        let actions = vec![
            read("/ws/a.rs", false),
            read("/etc/x", true),
            write("/ws/a.rs", false),
            write("/etc/x", true),
            edit("/ws/a.rs", false),
            edit("/etc/x", true),
            command(),
            mcp(),
        ];
        for action in &actions {
            for mode in modes {
                assert_eq!(
                    decide(action, mode, &ruleset_with(matching_rule(action, Effect::Allow))),
                    Decision::Allow,
                    "allow rule: {action:?} {mode:?}"
                );
                assert_eq!(
                    decide(action, mode, &ruleset_with(matching_rule(action, Effect::Ask))),
                    Decision::Ask,
                    "ask rule: {action:?} {mode:?}"
                );
                assert_eq!(
                    decide(action, mode, &ruleset_with(matching_rule(action, Effect::Deny))),
                    Decision::Deny,
                    // The headline case: deny survives even bypass (§1.3).
                    "deny rule: {action:?} {mode:?}"
                );
            }
        }
        // Free-pass tools: constant Allow, rules notwithstanding.
        for mode in modes {
            for effect in [Effect::Allow, Effect::Ask, Effect::Deny] {
                assert_eq!(
                    decide(
                        &PermissionAction::Trusted,
                        mode,
                        &ruleset_with(matching_rule(&PermissionAction::Trusted, effect))
                    ),
                    Decision::Allow
                );
            }
        }
    }

    /// When ask and allow rules both match, ask wins regardless of order
    /// (§1.3: fixed precedence, no specificity ranking); any deny in the
    /// chain wins over both.
    #[test]
    fn ask_beats_allow_and_deny_beats_all_regardless_of_order() {
        let mut ask_then_allow = plain_ruleset();
        ask_then_allow.add(matching_rule(&command(), Effect::Ask));
        ask_then_allow.add(matching_rule(&command(), Effect::Allow));
        assert_eq!(
            decide(&command(), PermissionMode::Default, &ask_then_allow),
            Decision::Ask
        );

        let mut allow_then_deny = plain_ruleset();
        allow_then_deny.add(matching_rule(&command(), Effect::Allow));
        allow_then_deny.add(matching_rule(&command(), Effect::Deny));
        assert_eq!(
            decide(&command(), PermissionMode::Bypass, &allow_then_deny),
            Decision::Deny
        );
    }

    /// A non-matching rule changes nothing: the mode default stands.
    #[test]
    fn non_matching_rule_falls_through_to_mode_default() {
        let rules = ruleset_with(Rule {
            effect: Effect::Allow,
            tool: "execute_command".into(),
            pattern: Some("cargo test".into()),
        });
        assert_eq!(
            decide(&command(), PermissionMode::Default, &rules),
            Decision::Ask
        );
        // And the same rule DOES cover its own prefix.
        let matching = PermissionAction::Command {
            command: "cargo test --all".into(),
        };
        assert_eq!(
            decide(&matching, PermissionMode::Default, &rules),
            Decision::Allow
        );
    }

    /// The built-in exec-root deny: file actions under the root are Deny in
    /// every mode, even with allow rules matching, and even under bypass;
    /// sibling directories and non-file classes are untouched.
    #[test]
    fn builtin_exec_dir_denies_file_actions_in_every_mode() {
        let modes = [
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::Bypass,
        ];
        // A nonexistent root: RuleSet anchors it absolutely without
        // creating anything (canonicalize simply falls back).
        let root = std::env::temp_dir().join("oct-perm-test-exec");
        let under = root.join("conv-id/bg_1.log");

        let mut allow_all_files = RuleSet::new(&root);
        for tool in ["read_file", "write_file", "edit_file"] {
            allow_all_files.add(Rule {
                effect: Effect::Allow,
                tool: tool.into(),
                pattern: None,
            });
        }

        for mode in modes {
            assert_eq!(
                decide(&read(&under.display().to_string(), false), mode, &allow_all_files),
                Decision::Deny,
                "read under exec root: {mode:?}"
            );
            assert_eq!(
                decide(&write(&under.display().to_string(), false), mode, &allow_all_files),
                Decision::Deny,
                "write under exec root: {mode:?}"
            );
            assert_eq!(
                decide(&edit(&under.display().to_string(), false), mode, &allow_all_files),
                Decision::Deny,
                "edit under exec root: {mode:?}"
            );
        }

        // A sibling directory sharing the prefix as a STRING is not under
        // the root (component-wise comparison, not string prefix): the
        // outside-read mode default (Ask) stands, not the built-in Deny.
        let bare = RuleSet::new(&root);
        let sibling = root.with_file_name("oct-perm-test-exec2").join("conv/bg.log");
        assert_eq!(
            decide(&read(&sibling.display().to_string(), true), PermissionMode::Default, &bare),
            Decision::Ask
        );

        // Non-file classes have no path: the built-in deny does not apply —
        // the command falls to its mode default (Ask on Default, Allow on
        // Bypass), never Deny.
        assert_eq!(
            decide(&command(), PermissionMode::Default, &allow_all_files),
            Decision::Ask
        );
        assert_eq!(
            decide(&command(), PermissionMode::Bypass, &bare),
            Decision::Allow
        );
    }

    #[test]
    fn rule_suggestions_are_conservative() {
        assert_eq!(
            rule_suggestion(&command()).as_deref(),
            Some("cargo build")
        );
        assert_eq!(
            rule_suggestion(&PermissionAction::Command {
                command: "npm".into()
            })
            .as_deref(),
            Some("npm")
        );
        // No pattern from an empty/whitespace command: an empty prefix
        // would match every command.
        assert_eq!(
            rule_suggestion(&PermissionAction::Command { command: "  ".into() }),
            None
        );

        assert_eq!(
            rule_suggestion(&write("/home/u/proj/src/main.rs", false)).as_deref(),
            Some("/home/u/proj/src/**")
        );
        assert_eq!(
            rule_suggestion(&read("/home/u/proj/notes.txt", true)).as_deref(),
            Some("/home/u/proj/**")
        );

        assert_eq!(rule_suggestion(&mcp()), None);
        assert_eq!(rule_suggestion(&PermissionAction::Trusted), None);
    }

    #[test]
    fn permission_mode_round_trips_db_and_serde_strings() {
        for mode in [
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::Bypass,
        ] {
            assert_eq!(PermissionMode::from_str_opt(mode.as_str()), Some(mode));
            // serde's snake_case and the DB string form are one form.
            let serde_str = serde_json::to_string(&mode).unwrap();
            assert_eq!(serde_str, format!("\"{}\"", mode.as_str()));
        }
        assert_eq!(PermissionMode::from_str_opt("yolo"), None);
    }
}
