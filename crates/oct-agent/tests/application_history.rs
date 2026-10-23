//! Durable history commands remain callable without HTTP or transport DTOs.
mod support;

use oct_agent::application::history::HistoryApplication;
use oct_agent::db::messages;
use oct_agent::error::AppError;
use oct_agent::history::{HistoryCut, HistorySide, RevertRequest};
use oct_llm_provider::core::{Message, Role};

#[tokio::test]
async fn committed_revert_replays_before_occupancy_and_conflicting_requests_still_fail() {
    let app = support::TestApp::new().await;
    let project = app.create_project().await;
    let source = app.create_conversation(&project).await;
    let mut ids = Vec::new();
    for (role, text) in [
        (Role::User, "first"),
        (Role::Assistant, "first answer"),
        (Role::User, "edit this"),
        (Role::Assistant, "old answer"),
    ] {
        ids.push(
            messages::insert_message(
                &app.pool,
                &source,
                &Message::text(role, text),
                None,
                None,
                None,
            )
            .await
            .unwrap()
            .id,
        );
    }
    let application = HistoryApplication::new(app.services.clone());
    let request = RevertRequest {
        operation_id: uuid::Uuid::new_v4().to_string(),
        expected_revision: 4,
        cut: HistoryCut::Message {
            message_id: ids[2].clone(),
            side: HistorySide::Before,
        },
    };
    let first = application.revert(&source, request.clone()).await.unwrap();
    let occupancy = app.services.occupancy().occupy(&source).unwrap();
    let replay = application.revert(&source, request.clone()).await.unwrap();
    assert_eq!(replay.backup_conversation_id, first.backup_conversation_id);
    assert_eq!(replay.context_revision, first.context_revision);
    assert_eq!(replay.draft.as_deref(), Some("edit this"));

    let different = RevertRequest {
        cut: HistoryCut::Message {
            message_id: ids[0].clone(),
            side: HistorySide::Before,
        },
        ..request.clone()
    };
    assert!(matches!(
        application.revert(&source, different).await,
        Err(AppError::OperationConflict(_))
    ));
    let new_operation = RevertRequest {
        operation_id: uuid::Uuid::new_v4().to_string(),
        expected_revision: first.context_revision,
        ..request
    };
    assert!(matches!(
        application.revert(&source, new_operation).await,
        Err(AppError::SessionBusy(_))
    ));
    assert_eq!(
        messages::list_messages(&app.pool, &source)
            .await
            .unwrap()
            .iter()
            .map(|row| &row.id)
            .collect::<Vec<_>>(),
        ids[..2].iter().collect::<Vec<_>>()
    );
    assert_eq!(
        messages::list_messages(&app.pool, &first.backup_conversation_id)
            .await
            .unwrap()
            .len(),
        4
    );
    drop(occupancy);
}
