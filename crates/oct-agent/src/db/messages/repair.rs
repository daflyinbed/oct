//! Compatibility repair for notifications persisted inside a tool batch.
use super::*;

/// Old send/resume paths could append a background notification before
/// repairing interrupted tools. Move only those synthetic messages after
/// their complete tool batch; content and per-slot revision stamps remain
/// unchanged. The caller holds conversation occupancy. One immediate
/// transaction protects both the read and the ordering/revision update.
pub async fn repair_interleaved_notifications(
    pool: &SqlitePool,
    conversation_id: &str,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let stored = list_messages_in_tx(&mut tx, conversation_id).await?;
    let mut sequence: Vec<usize> = (0..stored.len()).collect();
    let mut cursor = stored.iter().rposition(is_compaction_marker).unwrap_or(0);
    let mut changed = false;

    while cursor < stored.len() {
        let Ok(message) = stored[cursor].to_message() else {
            cursor += 1;
            continue;
        };
        let mut pending: HashSet<String> = message
            .parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::ToolCall(call) => Some(call.id.clone()),
                _ => None,
            })
            .collect();
        if message.role != Role::Assistant || pending.is_empty() {
            cursor += 1;
            continue;
        }

        let mut notifications = Vec::new();
        let mut results = Vec::new();
        let mut obstructed = false;
        let mut next = cursor + 1;
        while next < stored.len() && !pending.is_empty() {
            let row = &stored[next];
            if row.role == "user"
                && details_kind(row.details_json.as_deref()).as_deref()
                    == Some("background_task_notification")
            {
                notifications.push(next);
            } else if let Ok(message) = row.to_message() {
                // Do not reorder actual user input, markers, malformed rows,
                // new assistant turns, or unrelated/duplicate results.
                if message.role != Role::Tool || message.parts.is_empty() {
                    obstructed = true;
                    break;
                }
                let valid = message.parts.iter().all(|part| match part {
                    ContentPart::ToolResult(result) => pending.remove(&result.call_id),
                    _ => false,
                });
                if !valid {
                    obstructed = true;
                    break;
                }
                results.push(next);
            } else {
                obstructed = true;
                break;
            }
            next += 1;
        }
        if !obstructed && pending.is_empty() && !notifications.is_empty() {
            let reordered: Vec<_> = results.into_iter().chain(notifications).collect();
            sequence[cursor + 1..next].copy_from_slice(&reordered);
            changed = true;
            cursor = next;
        } else {
            cursor += 1;
        }
    }

    if changed {
        // Park existing positive slots first to avoid UNIQUE collisions;
        // reuse their exact values, preserving any intentional gaps.
        let first = stored.first().expect("a repaired batch has rows").ordering;
        sqlx::query!(
            "UPDATE conversation_messages SET ordering = -ordering WHERE conversation_id = $1 AND ordering >= $2",
            conversation_id,
            first,
        )
        .execute(&mut *tx)
        .await?;
        for (slot, index) in stored.iter().zip(sequence) {
            let id = &stored[index].id;
            sqlx::query!(
                "UPDATE conversation_messages SET ordering = $1 WHERE conversation_id = $2 AND message_id = $3",
                slot.ordering,
                conversation_id,
                id,
            )
            .execute(&mut *tx)
            .await?;
        }
        bump_revision(&mut tx, conversation_id).await?;
    }
    tx.commit().await?;
    Ok(changed)
}
