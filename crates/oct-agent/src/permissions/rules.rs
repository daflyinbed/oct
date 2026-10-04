//! In-memory rule model and matching semantics.
//!
//! A [`Rule`] is a `permission_rules` row minus its scoping columns — the
//! run-assembly loader decides WHICH rows a run evaluates against (project
//! + global + future conversation scope); matching here is purely
//! per-action.

use std::path::{Path, PathBuf};

use globset::GlobBuilder;

use crate::permissions::evaluator::PermissionAction;

/// What a rule does to a matching action, as persisted in
/// `permission_rules.effect`. Evaluation precedence is fixed:
/// deny > ask > allow > mode default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    Allow,
    Deny,
    Ask,
}

impl Effect {
    pub fn as_str(&self) -> &'static str {
        match self {
            Effect::Allow => "allow",
            Effect::Deny => "deny",
            Effect::Ask => "ask",
        }
    }

    /// Parse the DB/API string form; `None` for anything else. The column
    /// is CHECK-constrained, so the `None` arm is defense in depth (e.g. a
    /// hand-edited database).
    pub fn from_str_opt(value: &str) -> Option<Self> {
        match value {
            "allow" => Some(Effect::Allow),
            "deny" => Some(Effect::Deny),
            "ask" => Some(Effect::Ask),
            _ => None,
        }
    }
}

/// Canonical tool names (exactly what `Tool::name` reports) per action
/// class. The read tools share ONE class: the decision table treats
/// `read_file`/`list_dir`/`grep`/`glob` identically, so a rule naming any
/// one of them matches every read-class action.
const READ_CLASS_TOOLS: &[&str] = &["read_file", "list_dir", "grep", "glob"];
const WRITE_CLASS_TOOLS: &[&str] = &["write_file"];
const EDIT_CLASS_TOOLS: &[&str] = &["edit_file"];
const COMMAND_CLASS_TOOLS: &[&str] = &["execute_command"];

/// One permission rule in memory (a `permission_rules` row minus scope).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub effect: Effect,
    /// Exact tool name (e.g. `execute_command`, `mcp__github__create_issue`)
    /// or the documented server wildcard `mcp__{server}__*`.
    pub tool: String,
    /// `None` = any invocation of the tool; for `execute_command` a command
    /// string prefix; for the file tools an absolute-path glob.
    pub pattern: Option<String>,
}

impl Rule {
    /// Does this rule apply to the action? Tool/class first, then pattern
    /// under the matching class's semantics.
    pub fn matches(&self, action: &PermissionAction) -> bool {
        let tool_matches = match action {
            // Free-pass tools never build a descriptor: no rule can
            // name them, so nothing matches — not even a deny.
            PermissionAction::Trusted => false,
            PermissionAction::Mcp { tool } => mcp_tool_matches(&self.tool, tool),
            PermissionAction::Read { .. } => READ_CLASS_TOOLS.contains(&self.tool.as_str()),
            PermissionAction::Write { .. } => WRITE_CLASS_TOOLS.contains(&self.tool.as_str()),
            PermissionAction::Edit { .. } => EDIT_CLASS_TOOLS.contains(&self.tool.as_str()),
            PermissionAction::Command { .. } => COMMAND_CLASS_TOOLS.contains(&self.tool.as_str()),
        };
        if !tool_matches {
            return false;
        }
        match (&self.pattern, action) {
            // NULL pattern: any invocation of the matched tool.
            (None, _) => true,
            // execute_command: plain command-string prefix. Deliberately
            // NOT token-boundary-aware — a one-token rule like `cargo` is
            // broad on purpose; the rule_suggestion's two-token form exists
            // to keep "always allow" conservative.
            (Some(pattern), PermissionAction::Command { command }) => {
                command.starts_with(pattern.as_str())
            }
            // File tools: absolute-path glob.
            (Some(pattern), PermissionAction::Read { path, .. })
            | (Some(pattern), PermissionAction::Write { path, .. })
            | (Some(pattern), PermissionAction::Edit { path, .. }) => {
                path_glob_matches(pattern, path)
            }
            // A pattern on a class without pattern semantics (Mcp) never
            // matches: refuse rather than silently widen the rule.
            (Some(_), _) => false,
        }
    }
}

/// Tool-name matching for MCP actions: exact full name, or the
/// documented wildcard `mcp__{server}__*`. A trailing `*` is only honored
/// on `mcp__`-prefixed names; anywhere else it compares literally (no such
/// tool exists, so the rule is dead — visible in settings, inert at
/// evaluation).
fn mcp_tool_matches(rule_tool: &str, action_tool: &str) -> bool {
    if let Some(stem) = rule_tool.strip_suffix('*') {
        rule_tool.starts_with("mcp__") && action_tool.starts_with(stem)
    } else {
        rule_tool == action_tool
    }
}

/// Absolute-path glob matching via globset, with `*` confined to one path
/// segment and `**` crossing separators (the `{dir}/**` form rules and
/// rule suggestions use). An invalid pattern never matches — a typo'd rule
/// fails closed into asking.
fn path_glob_matches(pattern: &str, path: &Path) -> bool {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|glob| glob.compile_matcher().is_match(path))
        .unwrap_or(false)
}

/// The rules one run evaluates against: loaded from the DB once at
/// run assembly, extended live by `allow_always` approvals. Also carries
/// the background-task output root, because the built-in deny is
/// part of every evaluation and is not a row anyone could delete.
#[derive(Debug, Clone)]
pub struct RuleSet {
    rules: Vec<Rule>,
    /// The background-task output root (from
    /// [`crate::runtime::background::exec_output_root`]), best-effort
    /// anchored absolute so the built-in deny can prefix-match the
    /// canonicalized paths file actions carry.
    exec_root: PathBuf,
}

impl RuleSet {
    pub fn new(exec_root: impl Into<PathBuf>) -> Self {
        let raw = exec_root.into();
        // File actions carry canonicalized absolute paths, so a relative
        // root (a bare `oct.db` derives `exec`) must be anchored before any
        // prefix comparison. canonicalize also resolves symlinks on the
        // way; before the first background task the directory may not
        // exist yet, in which case CWD-anchoring is the best available
        // absolute form.
        let cwd = std::env::current_dir().unwrap_or_default();
        // Resolve existing ancestors too: the DB directory may be a symlink
        // while its exec child does not exist until the first command.
        let exec_root = crate::tools::workspace::WorkspaceRoots::new(cwd.clone(), Vec::new())
            .resolve_write_path(&raw.to_string_lossy())
            .map(|resolved| resolved.path)
            .unwrap_or_else(|_| {
                if raw.is_absolute() {
                    raw
                } else {
                    cwd.join(raw)
                }
            });
        Self {
            rules: Vec::new(),
            exec_root,
        }
    }

    pub fn add(&mut self, rule: Rule) -> &mut Self {
        self.rules.push(rule);
        self
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// The (anchored) background-task output root this set's built-in deny
    /// covers.
    pub fn exec_root(&self) -> &Path {
        &self.exec_root
    }

    /// Runtime output is readable, but cannot be modified even by bypass
    /// or an allow rule. Canonical paths keep symlink aliases equivalent.
    pub(crate) fn denies_exec_root(&self, action: &PermissionAction) -> bool {
        matches!(action, PermissionAction::Write { path, .. } | PermissionAction::Edit { path, .. }
            if self.under_exec_root(path))
    }

    fn under_exec_root(&self, path: &Path) -> bool {
        path.starts_with(&self.exec_root)
            || self
                .exec_root
                .canonicalize()
                .is_ok_and(|root| path.starts_with(root))
    }

    pub(crate) fn allows_exec_read(&self, action: &PermissionAction) -> bool {
        matches!(action, PermissionAction::Read { path, .. } if self.under_exec_root(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &str, outside: bool) -> PermissionAction {
        PermissionAction::Read {
            path: PathBuf::from(path),
            outside,
        }
    }

    fn rule(effect: Effect, tool: &str, pattern: Option<&str>) -> Rule {
        Rule {
            effect,
            tool: tool.to_string(),
            pattern: pattern.map(str::to_string),
        }
    }

    #[test]
    fn null_pattern_matches_any_invocation_of_the_tool() {
        let any = rule(Effect::Allow, "execute_command", None);
        assert!(any.matches(&PermissionAction::Command {
            command: "cargo build".into()
        }));
        assert!(any.matches(&PermissionAction::Command {
            command: "rm -rf /".into()
        }));
        // ...but only of THAT tool's class.
        assert!(!any.matches(&read("/tmp/a.rs", false)));
    }

    #[test]
    fn command_rules_match_by_string_prefix() {
        let r = rule(Effect::Allow, "execute_command", Some("cargo build"));
        assert!(r.matches(&PermissionAction::Command {
            command: "cargo build --workspace".into()
        }));
        assert!(!r.matches(&PermissionAction::Command {
            command: "cargo  build".into() // two spaces: not a prefix
        }));
        assert!(!r.matches(&PermissionAction::Command {
            command: "cargo run".into()
        }));
    }

    #[test]
    fn file_rules_match_by_absolute_path_glob() {
        let r = rule(Effect::Deny, "read_file", Some("/etc/ssh/**"));
        assert!(r.matches(&read("/etc/ssh/config", true)));
        assert!(r.matches(&read("/etc/ssh/extra/id_ed25519", true)));
        assert!(!r.matches(&read("/etc/hosts", true)));

        // `*` stays within one segment; `**` crosses.
        let single = rule(Effect::Allow, "write_file", Some("/tmp/*.log"));
        assert!(single.matches(&PermissionAction::Write {
            path: PathBuf::from("/tmp/a.log"),
            outside: true
        }));
        assert!(!single.matches(&PermissionAction::Write {
            path: PathBuf::from("/tmp/sub/a.log"),
            outside: true
        }));
        let deep = rule(Effect::Allow, "write_file", Some("/tmp/**"));
        assert!(deep.matches(&PermissionAction::Write {
            path: PathBuf::from("/tmp/x/y/z"),
            outside: true
        }));

        // A read-class rule matches every read-class action (the four
        // tools share one class).
        assert!(rule(Effect::Ask, "glob", Some("/etc/**")).matches(&read("/etc/passwd", true)));
        assert!(rule(Effect::Ask, "list_dir", Some("/etc/**")).matches(&read("/etc/passwd", true)));

        // Class isolation: file rules never match other classes.
        assert!(!r.matches(&PermissionAction::Command {
            command: "/etc/ssh/config".into()
        }));
        assert!(!rule(Effect::Allow, "write_file", Some("/etc/**"))
            .matches(&PermissionAction::Edit {
                path: PathBuf::from("/etc/hosts"),
                outside: true
            }));
    }

    #[test]
    fn mcp_rules_match_full_name_or_server_wildcard() {
        let exact = rule(Effect::Allow, "mcp__github__create_issue", None);
        assert!(exact.matches(&PermissionAction::Mcp {
            tool: "mcp__github__create_issue".into()
        }));
        assert!(!exact.matches(&PermissionAction::Mcp {
            tool: "mcp__github__close_issue".into()
        }));

        let wildcard = rule(Effect::Allow, "mcp__github__*", None);
        assert!(wildcard.matches(&PermissionAction::Mcp {
            tool: "mcp__github__create_issue".into()
        }));
        assert!(!wildcard.matches(&PermissionAction::Mcp {
            tool: "mcp__gitlab__create_issue".into()
        }));

        // A trailing `*` outside the documented mcp__ form compares
        // literally and therefore never matches.
        assert!(!rule(Effect::Allow, "read_*", None).matches(&read("/tmp/a", false)));

        // A pattern on an Mcp rule has no semantics: never matches.
        assert!(!rule(Effect::Allow, "mcp__github__*", Some("**"))
            .matches(&PermissionAction::Mcp {
                tool: "mcp__github__create_issue".into()
            }));

        // Non-mcp rules never match Mcp actions.
        assert!(!rule(Effect::Allow, "execute_command", None).matches(&PermissionAction::Mcp {
            tool: "mcp__github__create_issue".into()
        }));
    }

    #[test]
    fn invalid_glob_patterns_never_match() {
        let bad = rule(Effect::Allow, "write_file", Some("/tmp/[oops"));
        assert!(!bad.matches(&PermissionAction::Write {
            path: PathBuf::from("/tmp/a"),
            outside: true
        }));
    }

    #[test]
    fn trusted_actions_match_no_rule() {
        assert!(!rule(Effect::Deny, "task", None).matches(&PermissionAction::Trusted));
        assert!(!rule(Effect::Deny, "task_stop", Some("**")).matches(&PermissionAction::Trusted));
    }

    #[test]
    fn effect_round_trips_db_strings() {
        for effect in [Effect::Allow, Effect::Deny, Effect::Ask] {
            assert_eq!(Effect::from_str_opt(effect.as_str()), Some(effect));
        }
        assert_eq!(Effect::from_str_opt("maybe"), None);
    }

    #[test]
    fn relative_exec_root_is_cwd_anchored() {
        let set = RuleSet::new("some-exec-dir");
        let anchored = std::env::current_dir()
            .unwrap()
            .join("some-exec-dir");
        assert_eq!(set.exec_root(), anchored);
        assert!(set.allows_exec_read(&read(
            &anchored.join("conv/bg_1.log").display().to_string(),
            true,
        )));
    }

    #[cfg(unix)]
    #[test]
    fn nonexistent_exec_root_under_a_symlink_resolves_its_real_boundary() {
        let parent = tempfile::tempdir().unwrap();
        let real = tempfile::tempdir().unwrap();
        let alias = parent.path().join("db");
        std::os::unix::fs::symlink(real.path(), &alias).unwrap();
        let rules = RuleSet::new(alias.join("exec"));
        let path = real.path().join("exec/conv/output.log");
        assert_eq!(rules.exec_root(), real.path().join("exec"));
        assert!(rules.denies_exec_root(&PermissionAction::Write {
            path: path.clone(),
            outside: true
        }));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        assert!(rules.allows_exec_read(&PermissionAction::Read {
            path,
            outside: true
        }));
    }
}
