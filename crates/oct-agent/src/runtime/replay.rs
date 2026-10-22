//! The durable base of an execution's in-memory event log. Capture after
//! preparation, then publish exactly once before starting model/tool output.

use sqlx::SqlitePool;

use crate::agent::AgentEvent;
use crate::error::AppResult;

pub(super) struct ReplayBase {
    revision: i64,
    history_epoch: i64,
    pub trigger_message_id: Option<String>,
}

impl ReplayBase {
    pub async fn load(pool: &SqlitePool, conversation_id: &str) -> AppResult<Self> {
        // One SQLite statement observes the revision, branch and trigger in
        // the same read snapshot. Compaction markers do not start UI turns.
        let row = sqlx::query!(
            r#"SELECT c.context_revision, c.history_epoch,
                (SELECT m.id FROM conversation_messages cm
                 JOIN messages m ON m.id = cm.message_id
                 WHERE cm.conversation_id = c.id AND m.role = 'user'
                   AND COALESCE(CASE WHEN json_valid(m.details_json) THEN json_extract(m.details_json, '$.kind') END, '') != 'compaction'
                 ORDER BY cm.ordering DESC LIMIT 1) AS "trigger_message_id: String"
               FROM conversations c WHERE c.id = $1"#,
            conversation_id,
        )
        .fetch_one(pool)
        .await?;
        Ok(Self {
            revision: row.context_revision,
            history_epoch: row.history_epoch,
            trigger_message_id: row.trigger_message_id,
        })
    }

    pub fn event(self, run_id: &str, execution_epoch: i64) -> AgentEvent {
        AgentEvent::RunMeta {
            run_id: run_id.to_owned(),
            execution_epoch,
            history_epoch: self.history_epoch,
            replay_after_revision: self.revision,
            trigger_message_id: self.trigger_message_id,
        }
    }
}
