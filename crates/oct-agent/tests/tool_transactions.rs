//! A sibling tool must progress while another result is being persisted.
mod support;

use async_trait::async_trait;
use oct_agent::events::AgentEvent;
use oct_agent::tool_contract::AgentTool;
use oct_agent::tool_contract::ToolContext;
use oct_agent::tool_contract::ToolOutput;
use oct_llm_provider::core::{ContentPart, FinishReason};
use serde_json::{Value, json};
use std::sync::Arc;
use support::{
    ScriptedModel, TestEnv, Turn, collect_events, finish, is_terminal, new_conversation,
    spawn_agent, tool_call,
};
use tokio::sync::Notify;

struct TransactionTool {
    pool: sqlx::SqlitePool,
    acquired: Arc<Notify>,
    sibling_ready: Arc<Notify>,
}
#[async_trait]
impl AgentTool for TransactionTool {
    fn name(&self) -> &str {
        "transaction"
    }
    fn description(&self) -> &str {
        "hold a write transaction across an await"
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object"})
    }
    async fn execute(&self, _: Value, _: &ToolContext) -> anyhow::Result<ToolOutput> {
        let tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        self.acquired.notify_one();
        self.sibling_ready.notified().await;
        // The sibling has returned while we still own SQLite's writer lock.
        // A parent that stops polling us to persist that result deadlocks.
        tokio::task::yield_now().await;
        tx.commit().await?;
        Ok(ToolOutput::success("committed"))
    }
}
struct SignalTool {
    acquired: Arc<Notify>,
    sibling_ready: Arc<Notify>,
}
#[async_trait]
impl AgentTool for SignalTool {
    fn name(&self) -> &str {
        "signal"
    }
    fn description(&self) -> &str {
        "return while the sibling holds its transaction"
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object"})
    }
    async fn execute(&self, _: Value, _: &ToolContext) -> anyhow::Result<ToolOutput> {
        self.acquired.notified().await;
        self.sibling_ready.notify_one();
        Ok(ToolOutput::success("signalled"))
    }
}

#[tokio::test]
async fn persist_completed_result_does_not_stop_sibling_transaction() {
    let env = TestEnv::new().await;
    let conv = new_conversation(&env.pool, env.dir.path()).await;
    let acquired = Arc::new(Notify::new());
    let sibling_ready = Arc::new(Notify::new());
    let model = ScriptedModel::new(vec![
        Turn::Stream(vec![
            tool_call("a", "transaction", json!({})),
            tool_call("b", "signal", json!({})),
            finish(FinishReason::ToolCalls),
        ]),
        Turn::Stream(vec![finish(FinishReason::Stop)]),
    ]);
    let (_handle, mut events) = spawn_agent(
        model,
        vec![
            Box::new(TransactionTool {
                pool: env.pool.clone(),
                acquired: acquired.clone(),
                sibling_ready: sibling_ready.clone(),
            }),
            Box::new(SignalTool {
                acquired,
                sibling_ready,
            }),
        ],
        env.pool.clone(),
        &conv,
        "go",
    )
    .await;
    let events = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        collect_events(&mut events, is_terminal),
    )
    .await
    .expect("sibling transaction stalled");
    assert!(matches!(events.last(), Some(AgentEvent::Finish)));
    let messages = oct_agent::db::messages::list_messages(&env.pool, &conv)
        .await
        .unwrap();
    let results: Vec<_> = messages.iter().filter(|m| m.role == "tool").collect();
    assert_eq!(results.len(), 2);
    for result in results {
        let parts: Vec<ContentPart> = serde_json::from_str(&result.parts_json).unwrap();
        assert!(matches!(&parts[0], ContentPart::ToolResult(r) if !r.is_error));
    }
}
