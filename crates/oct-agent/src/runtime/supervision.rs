//! Run ownership and durable settlement, independent of HTTP.
use super::*;

/// Derive a worker's toolset from the parent's live set:
/// parent set − `task`, with `read_file`/`write_file`/`edit_file` rebuilt
/// over the shared locks and a fresh read state. Everything else (stateless
/// builtins, MCP tools) is Arc-cloned so connections and configs are reused
/// verbatim — schema AND dispatch stay exactly parent-minus-task.
///
/// `execute_command` is likewise rebuilt (background gate closed, schema
/// identical): subagent workers must not detach commands — the wake-run
/// semantics only hold in the root conversation. The background
/// companions (`task_output`/`task_stop`) are scoped to the root
/// conversation and simply dropped.
pub(crate) fn derive_child_toolset(
    parent: &[Arc<dyn tools::AgentTool>],
    roots: WorkspaceRoots,
    locks: Arc<WorkspaceFileLocks>,
) -> Vec<Arc<dyn tools::AgentTool>> {
    let mut child: Vec<Arc<dyn tools::AgentTool>> =
        tools::file_tools_with(roots.clone(), locks, Arc::new(AgentReadState::new()))
            .into_iter()
            .map(Arc::from)
            .collect();
    child.push(Arc::new(tools::execute::ExecuteCommandTool::new(roots)) as Arc<dyn tools::AgentTool>);
    const NOT_REUSED: [&str; 7] = [
        "task",
        "read_file",
        "write_file",
        "edit_file",
        "execute_command",
        "task_output",
        "task_stop",
    ];
    child.extend(
        parent
            .iter()
            .filter(|t| !NOT_REUSED.contains(&t.name()))
            .cloned(),
    );
    child
}

/// The run supervisor: await the loop, persist the terminal
/// state, then release the conversation's occupancy — in that order, so a
/// new run can never start before the previous one's outcome is on disk.
pub(super) async fn supervise(
    pool: SqlitePool,
    run_id: String,
    conv_id: String,
    approvals: Arc<ApprovalRegistry>,
    guard: OccupancyGuard,
    ctx: AgentContext,
    view: LlmView,
    handle: RunHandle,
) {
    // The loop runs in its OWN task: a panic inside it resolves this await
    // as a JoinError and becomes a Failed outcome here, instead of unwinding
    // past the supervisor and skipping both the terminal write and the
    // occupancy release. The hub is cloned out first — the approval sweep
    // below publishes on it after the loop (and with it `handle`) is gone.
    let hub = handle.hub.clone();
    let outcome = match tokio::spawn(wire::with_tags(
        wire::Tags {
            conversation_id: Some(conv_id.clone()),
            run_id: Some(run_id.clone()),
            run_kind: Some("root".to_string()),
            purpose: None,
        },
        run_agent_loop(ctx, conv_id.clone(), view, handle),
    ))
    .await
    {
        Ok(outcome) => outcome,
        Err(join_err) => RunOutcome {
            status: RunStatus::Failed,
            final_message_id: None,
            final_text: None,
            partial: false,
            usage: None,
            error: Some(format!("agent loop panicked: {join_err}")),
        },
    };

    // The async terminal write lives here — never in Drop (a guard's Drop
    // cannot await). The `status IN ('running','lost')` guard in the UPDATE
    // makes settlement one-shot. Transient storage failures retain ownership
    // and retry, preventing a new run from racing an uncommitted outcome.
    let final_status = match outcome.status {
        RunStatus::Completed => run_db::RunFinalStatus::Completed,
        RunStatus::Failed => run_db::RunFinalStatus::Failed,
        RunStatus::Cancelled => run_db::RunFinalStatus::Cancelled,
    };
    if let Err(e) = run_db::finish_run(
        &pool,
        &run_id,
        final_status,
        outcome.final_message_id.as_deref(),
        outcome.error.as_deref(),
        outcome
            .usage
            .as_ref()
            .map(|u| i64::try_from(u.input_tokens).unwrap_or(i64::MAX)),
        outcome
            .usage
            .as_ref()
            .map(|u| i64::try_from(u.output_tokens).unwrap_or(i64::MAX)),
        // Root runs never resume their own row (recovery starts a NEW root
        // run), so no epoch guard is needed here.
        None,
    )
    .await
    {
        tracing::error!("failed to persist terminal state of run {run_id}: {e}");
    }

    // Pending-approval cleanup: whatever the
    // gate futures could not settle themselves (a cancel drops them
    // mid-await) resolves here — published while the hub is still alive so
    // subscribers see the closed cards before the stream ends.
    publish_cancelled_approvals(approvals.as_ref(), &run_id, &hub);

    // Release occupancy LAST: dropping the guard removes the session entry,
    // the last hub `Arc` goes away, and every SSE stream reaches EOF — the
    // same termination semantics the pre-RunService inline cleanup had.
    drop(guard);
}

/// The subagent-run supervisor: same shape as [`supervise`], but the
/// settlement goes through [`SubagentManager::settle`] (epoch-guarded
/// finish + `subagent_updated` on the parent hub + done watch + memory
/// cleanup) and runs BEFORE the guard drop, so a watcher released by the
/// watch always finds the terminal row in the DB.
#[allow(clippy::too_many_arguments)]
pub(super) async fn supervise_child(
    pool: SqlitePool,
    approvals: Arc<ApprovalRegistry>,
    subagents: Arc<SubagentManager>,
    run_id: String,
    conv_id: String,
    agent_id: String,
    description: String,
    epoch: i64,
    parent_call_id: String,
    guard: OccupancyGuard,
    ctx: AgentContext,
    view: LlmView,
    handle: RunHandle,
    parent_hub: Arc<EventHub>,
    done_tx: watch::Sender<bool>,
) {
    let outcome = match tokio::spawn(wire::with_tags(
        wire::Tags {
            conversation_id: Some(conv_id.clone()),
            run_id: Some(run_id.clone()),
            run_kind: Some("subagent".to_string()),
            purpose: None,
        },
        run_agent_loop(ctx, conv_id.clone(), view, handle.clone()),
    ))
    .await
    {
        Ok(outcome) => outcome,
        Err(join_err) => RunOutcome {
            status: RunStatus::Failed,
            final_message_id: None,
            final_text: None,
            partial: false,
            usage: None,
            error: Some(format!("agent loop panicked: {join_err}")),
        },
    };

    // Same pending-approval sweep as the root supervisor — uniformity over
    // the (currently unreachable) subagent Ask arm: children downgrade Ask
    // to Deny, but the sweep is what makes that structurally guaranteed.
    publish_cancelled_approvals(approvals.as_ref(), &run_id, &handle.hub);

    subagents
        .settle(
            &pool,
            &run_id,
            &agent_id,
            &description,
            epoch,
            &parent_call_id,
            &outcome,
            &parent_hub,
            &handle.hub,
            &done_tx,
        )
        .await;

    // Release the child's occupancy LAST (settlement is already on disk, the
    // done watch is closed): the session entry goes away, the last strong
    // `Arc` to the child hub drops, and the child's SSE streams reach EOF.
    drop(guard);
}
