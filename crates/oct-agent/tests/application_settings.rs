//! Settings commands retain their editing, execution and transactional boundaries.
mod support;

use oct_agent::agent_settings::{ModelChoice, Patch, SettingsPatch, ToolChoice};
use oct_agent::application::settings::SettingsApplication;
use oct_agent::error::AppError;
use serde_json::json;

#[tokio::test]
async fn stale_model_remains_editable_but_cannot_run_or_partially_update() {
    let app = support::TestApp::new().await;
    let project = app.create_project().await;
    let llm = support::llm::spawn_llm_mock(vec![]).await;
    app.register_llm_provider(&llm).await;
    assert_eq!(
        app.post_json(
            "/api/providers/mock-llm/models",
            json!({
                "model_id":"test-model","name":"test-model","tool_call":true
            })
        )
        .await
        .status(),
        201
    );
    let settings = SettingsApplication::new(app.pool.clone());
    settings
        .update(
            "worker",
            Some(&project),
            SettingsPatch {
                model: Patch::Set(ModelChoice::Fixed {
                    provider_id: "mock-llm".into(),
                    model_id: "test-model".into(),
                }),
                instructions: Patch::Set("keep me".into()),
                ..Default::default()
            },
            &app.mcp,
        )
        .await
        .unwrap();
    assert!(
        oct_agent::db::models::delete_model(&app.pool, "mock-llm", "test-model")
            .await
            .unwrap()
    );

    let editor = settings.read("worker", Some(&project)).await.unwrap();
    assert!(matches!(editor.effective.model, ModelChoice::Fixed { .. }));
    assert!(
        matches!(settings.read_for_run("worker", Some(&project)).await,
        Err(AppError::BadRequest(message)) if message == "Configured model does not exist")
    );
    assert!(
        settings
            .update(
                "worker",
                Some(&project),
                SettingsPatch {
                    instructions: Patch::Set("must roll back".into()),
                    ..Default::default()
                },
                &app.mcp
            )
            .await
            .is_err()
    );
    assert_eq!(
        settings
            .read("worker", Some(&project))
            .await
            .unwrap()
            .overrides
            .instructions
            .as_deref(),
        Some("keep me")
    );

    let repaired = settings
        .update(
            "worker",
            Some(&project),
            SettingsPatch {
                model: Patch::Inherit,
                ..Default::default()
            },
            &app.mcp,
        )
        .await
        .unwrap();
    assert_eq!(repaired.overrides.model, None);
    assert_eq!(repaired.effective.model, ModelChoice::Parent);
    assert_eq!(repaired.effective.instructions, "keep me");
    settings
        .read_for_run("worker", Some(&project))
        .await
        .unwrap();
    assert!(llm.requests().is_empty());
}

#[tokio::test]
async fn concurrent_global_and_project_updates_keep_field_inheritance() {
    let app = support::TestApp::new().await;
    let project = app.create_project().await;
    let settings = SettingsApplication::new(app.pool.clone());
    settings
        .update(
            "worker",
            None,
            SettingsPatch {
                instructions: Patch::Set("old global".into()),
                ..Default::default()
            },
            &app.mcp,
        )
        .await
        .unwrap();

    let (global, local) = tokio::join!(
        settings.update(
            "worker",
            None,
            SettingsPatch {
                instructions: Patch::Set("new global".into()),
                ..Default::default()
            },
            &app.mcp
        ),
        settings.update(
            "worker",
            Some(&project),
            SettingsPatch {
                tools: Patch::Set(ToolChoice::Allowlist { items: vec![] }),
                ..Default::default()
            },
            &app.mcp
        )
    );
    global.unwrap();
    local.unwrap();
    let value = settings
        .read_for_run("worker", Some(&project))
        .await
        .unwrap();
    assert_eq!(value.effective.instructions, "new global");
    assert_eq!(value.sources.instructions, "global");
    assert_eq!(value.overrides.instructions, None);
    assert_eq!(
        value.effective.tools,
        ToolChoice::Allowlist { items: vec![] }
    );
    assert_eq!(value.sources.tools, "project");
}
