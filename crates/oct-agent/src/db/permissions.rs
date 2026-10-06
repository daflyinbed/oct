use anyhow::Result;
use chrono::NaiveDateTime;
use sqlx::SqlitePool;
use uuid::Uuid;

/// One persisted permission rule: the allow/deny/ask chain
/// every tool call is evaluated against. Rows are loaded once at run
/// assembly into an in-memory `permissions::rules::RuleSet`; the settings
/// page and "always allow" approvals write them here.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PermissionRule {
    pub id: String,
    /// NULL = a global rule (applies to every project).
    pub project_id: Option<String>,
    /// NULL = not conversation-scoped. Reserved for future "always allow
    /// within this conversation" rules; nothing writes it yet.
    pub conversation_id: Option<String>,
    /// `allow` | `deny` | `ask` (CHECK-constrained by the migration).
    pub effect: String,
    /// Exact tool name (e.g. `execute_command`) or `mcp__{server}__*`.
    pub tool: String,
    /// NULL = any invocation of the tool; command prefix for
    /// `execute_command`; absolute-path glob for the file tools.
    pub pattern: Option<String>,
    /// `user` (settings page) or `approval` (an "always allow" answer).
    pub source: String,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

/// Insert payload for a new rule. `effect`/`source` are validated by the
/// table's CHECK constraints (same policy as the rest of this crate: the
/// storage layer is the guard, callers map the error).
pub struct NewPermissionRule<'a> {
    /// NULL = global rule.
    pub project_id: Option<&'a str>,
    pub conversation_id: Option<&'a str>,
    pub effect: &'a str,
    pub tool: &'a str,
    pub pattern: Option<&'a str>,
    pub source: &'a str,
}

pub async fn create_rule(pool: &SqlitePool, new: NewPermissionRule<'_>) -> Result<PermissionRule> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().naive_utc();

    sqlx::query!(
        "INSERT INTO permission_rules (id, project_id, conversation_id, effect, tool, pattern, source, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8)",
        id,
        new.project_id,
        new.conversation_id,
        new.effect,
        new.tool,
        new.pattern,
        new.source,
        now,
    )
    .execute(pool)
    .await?;

    Ok(PermissionRule {
        id,
        project_id: new.project_id.map(str::to_string),
        conversation_id: new.conversation_id.map(str::to_string),
        effect: new.effect.to_string(),
        tool: new.tool.to_string(),
        pattern: new.pattern.map(str::to_string),
        source: new.source.to_string(),
        created_at: now,
        updated_at: now,
    })
}

/// The rules visible to a project: its own rows plus the global
/// (`project_id IS NULL`) ones, oldest first. Conversation-scoped rows are
/// deliberately excluded — they belong to a single conversation's gate, not
/// to the project settings list that reads this.
pub async fn list_rules_for_project(
    pool: &SqlitePool,
    project_id: &str,
) -> Result<Vec<PermissionRule>> {
    let rows = sqlx::query_as!(
        PermissionRule,
        "SELECT id, project_id, conversation_id, effect, tool, pattern, source, created_at, updated_at FROM permission_rules WHERE (project_id = $1 OR project_id IS NULL) AND conversation_id IS NULL ORDER BY created_at ASC",
        project_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// The GLOBAL rules only (`project_id IS NULL`): the settings page's
/// global-scope list — the counterpart of [`list_rules_for_project`] for
/// the `/api/permission-rules` surface.
pub async fn list_global_rules(pool: &SqlitePool) -> Result<Vec<PermissionRule>> {
    let rows = sqlx::query_as!(
        PermissionRule,
        "SELECT id, project_id, conversation_id, effect, tool, pattern, source, created_at, updated_at FROM permission_rules WHERE project_id IS NULL AND conversation_id IS NULL ORDER BY created_at ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn delete_rule(pool: &SqlitePool, id: &str) -> Result<bool> {
    let result = sqlx::query!("DELETE FROM permission_rules WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Row lifecycle over a migrated temp-file database: global and project
    /// rows both land in a project's list, other projects only see the
    /// global ones, conversation-scoped rows stay out, CHECK constraints
    /// reject bad effect/source, delete is a one-shot bool.
    #[tokio::test]
    async fn rule_row_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", dir.path().join("t.db").display());
        let pool = crate::db::init_pool(&url).await.unwrap();

        let project = crate::db::projects::create_project(
            &pool,
            &crate::db::projects::CreateProjectRequest {
                name: "p".into(),
                working_dir: dir.path().display().to_string(),
                additional_dirs: None,
            },
        )
        .await
        .unwrap();
        let other = crate::db::projects::create_project(
            &pool,
            &crate::db::projects::CreateProjectRequest {
                name: "q".into(),
                working_dir: dir.path().display().to_string(),
                additional_dirs: None,
            },
        )
        .await
        .unwrap();
        let conv = crate::db::conversations::create_conversation(&pool, &project.id, Some("c1"))
            .await
            .unwrap();

        let global = create_rule(
            &pool,
            NewPermissionRule {
                project_id: None,
                conversation_id: None,
                effect: "allow",
                tool: "execute_command",
                pattern: Some("cargo build"),
                source: "user",
            },
        )
        .await
        .unwrap();
        let scoped = create_rule(
            &pool,
            NewPermissionRule {
                project_id: Some(&project.id),
                conversation_id: None,
                effect: "deny",
                tool: "read_file",
                pattern: Some("/etc/**"),
                source: "approval",
            },
        )
        .await
        .unwrap();
        let conv_scoped = create_rule(
            &pool,
            NewPermissionRule {
                project_id: Some(&project.id),
                conversation_id: Some(&conv.id),
                effect: "ask",
                tool: "mcp__github__*",
                pattern: None,
                source: "user",
            },
        )
        .await
        .unwrap();

        // The project sees its own rows plus the global one, oldest first.
        let listed = list_rules_for_project(&pool, &project.id).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, global.id);
        assert_eq!(listed[0].pattern.as_deref(), Some("cargo build"));
        assert_eq!(listed[1].id, scoped.id);
        assert_eq!(listed[1].effect, "deny");

        // Another project only sees the global rule.
        let other_listed = list_rules_for_project(&pool, &other.id).await.unwrap();
        assert_eq!(other_listed.len(), 1);
        assert_eq!(other_listed[0].id, global.id);

        // The CHECK constraints reject unknown effect/source values.
        assert!(
            create_rule(
                &pool,
                NewPermissionRule {
                    project_id: None,
                    conversation_id: None,
                    effect: "maybe",
                    tool: "execute_command",
                    pattern: None,
                    source: "user",
                },
            )
            .await
            .is_err()
        );
        assert!(
            create_rule(
                &pool,
                NewPermissionRule {
                    project_id: None,
                    conversation_id: None,
                    effect: "allow",
                    tool: "execute_command",
                    pattern: None,
                    source: "wizard",
                },
            )
            .await
            .is_err()
        );

        // Delete: one-shot bool, conversation-scoped rows delete the same way.
        assert!(delete_rule(&pool, &scoped.id).await.unwrap());
        assert!(!delete_rule(&pool, &scoped.id).await.unwrap());
        assert!(delete_rule(&pool, &conv_scoped.id).await.unwrap());
        assert_eq!(
            list_rules_for_project(&pool, &project.id)
                .await
                .unwrap()
                .len(),
            1
        );
    }
}
