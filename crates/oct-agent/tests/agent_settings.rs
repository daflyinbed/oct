//! Configuration and real request/dispatch acceptance, with local models only.
mod support;
use futures_util::StreamExt;
use serde_json::{Value, json};
use support::TestApp;
use support::llm::{LlmTurn, TitleTurn, finish_chunk, spawn_llm_mock, text_chunk, tool_call_chunk};

async fn collect_events(response: reqwest::Response) -> Vec<Value> {
    assert_eq!(response.status(), 200);
    let mut stream = support::sse_payloads(response);
    let mut events = vec![];
    while let Some(event) = tokio::time::timeout(std::time::Duration::from_secs(30), stream.next())
        .await
        .unwrap()
    {
        events.push(event);
    }
    events
}

async fn patch(app: &TestApp, path: &str, value: Value) -> Value {
    let response = app.patch_json(path, value).await;
    assert_eq!(response.status(), 200);
    response.json().await.unwrap()
}

async fn add_model(app: &TestApp, id: &str, tool_call: bool) {
    let response = app
        .post_json(
            "/api/providers/mock-llm/models",
            json!({"model_id":id,"name":id,"tool_call":tool_call,"limit_context":12345}),
        )
        .await;
    assert_eq!(response.status(), 201);
}

#[tokio::test]
async fn overrides_resets_empty_tools_and_validation_are_atomic() {
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let global = "/api/settings/agents/worker";
    let local = format!("/api/projects/{project}/settings/agents/worker");
    let default: Value = app.get_json(&local).await.json().await.unwrap();
    assert_eq!(default["effective"]["tools"]["mode"], "parent");
    patch(&app, global, json!({"instructions":"global"})).await;
    let value = patch(
        &app,
        &local,
        json!({"instructions":"", "tools":{"mode":"allowlist","items":[]}}),
    )
    .await;
    assert_eq!(value["effective"]["instructions"], "");
    assert_eq!(value["sources"]["model"], "builtin");
    let llm = spawn_llm_mock(vec![]).await;
    app.register_llm_provider(&llm).await;
    add_model(&app, "test-model", true).await;
    patch(
        &app,
        global,
        json!({"model":{"mode":"fixed","provider_id":"mock-llm","model_id":"test-model"}}),
    )
    .await;
    let omitted = patch(&app, &local, json!({"model":{"mode":"parent"}})).await;
    assert_eq!(omitted["effective"]["model"]["mode"], "parent");
    assert_eq!(omitted["sources"]["model"], "project");
    assert_eq!(omitted["effective"]["tools"]["items"], json!([]));
    for invalid in [
        json!({"instructions":"changed","tools":{"mode":"allowlist","items":[{"kind":"builtin","name":"task"}]}}),
        json!({"tools":{"mode":"allowlist","items":[{"kind":"builtin","name":"edit_file"}]}}),
        json!({"model":{"mode":"fixed","provider_id":"missing","model_id":"missing"}}),
        json!({"instructions":"x".repeat(32769)}),
        json!({"unknown":true}),
    ] {
        assert_eq!(app.patch_json(&local, invalid).await.status(), 400);
    }
    let unchanged: Value = app.get_json(&local).await.json().await.unwrap();
    assert_eq!(unchanged["effective"], omitted["effective"]);
    let inherited = patch(&app, &local, json!({"instructions":null,"tools":null})).await;
    assert_eq!(inherited["effective"]["instructions"], "global");
    assert_eq!(inherited["sources"]["instructions"], "global");
    assert_eq!(inherited["effective"]["tools"]["mode"], "parent");
    assert_eq!(
        app.patch_json(
            "/api/settings/agents/title",
            json!({"tools":{"mode":"parent"}})
        )
        .await
        .status(),
        400
    );
    assert_eq!(
        app.get_json("/api/settings/agents/unknown").await.status(),
        404
    );
}

#[tokio::test]
async fn tool_catalog_and_mcp_choices_respect_scope_and_original_identity() {
    use std::sync::Arc;
    use support::mcp::{FakeMcpConnection, fake_tool};
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let other = app.create_project().await;
    let mut ids = vec![];
    for (name, scope) in [("global-search", None), ("project-search", Some(&project))] {
        let response = app
            .post_json(
                "/api/mcp/servers",
                json!({"name":name,"transport":"stdio","command":"not-spawned","project_id":scope}),
            )
            .await;
        assert_eq!(response.status(), 201);
        let row: Value = response.json().await.unwrap();
        let id = row["id"].as_str().unwrap().to_string();
        let tools = vec![fake_tool("search", Some("original search"))];
        app.mcp.insert_connection_for_test(
            &id,
            name,
            scope.map(String::as_str),
            Arc::new(FakeMcpConnection::new(tools.clone())),
            tools,
        );
        ids.push(id);
    }
    for (path, expected) in [
        ("/api/settings/agent-tools".into(), vec![ids[0].clone()]),
        (
            format!("/api/projects/{project}/settings/agent-tools"),
            ids.clone(),
        ),
        (
            format!("/api/projects/{other}/settings/agent-tools"),
            vec![ids[0].clone()],
        ),
    ] {
        let values: Vec<Value> = app.get_json(&path).await.json().await.unwrap();
        let mut actual: Vec<_> = values
            .iter()
            .filter_map(|v| v["selector"]["server_id"].as_str().map(str::to_owned))
            .collect();
        actual.sort();
        let mut expected = expected;
        expected.sort();
        assert_eq!(actual, expected);
        assert!(values.iter().all(|v| v["available"] == true));
    }
    let selection = |id: &str, tool: &str| json!({"tools":{"mode":"allowlist","items":[{"kind":"mcp","server_id":id,"tool":tool}]}});
    assert_eq!(
        app.patch_json("/api/settings/agents/worker", selection(&ids[1], "search"))
            .await
            .status(),
        400
    );
    assert_eq!(
        app.patch_json("/api/settings/agents/worker", selection(&ids[0], "missing"))
            .await
            .status(),
        400
    );
    let saved = patch(
        &app,
        &format!("/api/projects/{project}/settings/agents/worker"),
        selection(&ids[1], "search"),
    )
    .await;
    assert_eq!(
        saved["effective"]["tools"]["items"][0],
        json!({"kind":"mcp","server_id":ids[1],"tool":"search"})
    );
}

#[tokio::test]
async fn worker_selects_model_and_only_dispatches_selected_tools() {
    let llm = spawn_llm_mock(vec![
        LlmTurn::Complete(vec![
            tool_call_chunk(
                "task1",
                "task",
                &json!({"description":"restricted","prompt":"CONFIG-WORKER"}).to_string(),
            ),
            finish_chunk("tool_calls"),
        ]),
        LlmTurn::Complete(vec![text_chunk("parent done"), finish_chunk("stop")]),
    ])
    .await;
    llm.push_marker_turns(
        "CONFIG-WORKER",
        [
            LlmTurn::Complete(vec![
                tool_call_chunk(
                    "forbidden",
                    "write_file",
                    &json!({"path":"canary.txt","content":"bad"}).to_string(),
                ),
                finish_chunk("tool_calls"),
            ]),
            LlmTurn::Complete(vec![text_chunk("worker done"), finish_chunk("stop")]),
        ],
    );
    let app = TestApp::new().await;
    app.register_llm_provider(&llm).await;
    add_model(&app, "worker-model", true).await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;
    patch(
        &app,
        "/api/settings/agents/worker",
        json!({"instructions":"GLOBAL-WORKER-STYLE"}),
    )
    .await;
    patch(&app, &format!("/api/projects/{project}/settings/agents/worker"), json!({
        "model":{"mode":"fixed","provider_id":"mock-llm","model_id":"worker-model"},
        "tools":{"mode":"allowlist","items":[{"kind":"builtin","name":"read_file"},{"kind":"builtin","name":"grep"}]}
    })).await;
    let events = collect_events(app.send_message(&conv, "run").await).await;
    assert!(!events.iter().any(|e| e["type"] == "approval_requested"));
    assert!(!app.dir.path().join("canary.txt").exists());
    let requests = llm.requests();
    let child: Vec<_> = requests
        .iter()
        .filter(|r| r["model"] == "worker-model")
        .collect();
    assert_eq!(child.len(), 2);
    for r in child {
        let names: Vec<_> = r["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["read_file", "grep"]);
        assert!(
            r["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("GLOBAL-WORKER-STYLE")
        );
        assert!(
            !r["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("Use execute_command")
        );
    }
    assert!(requests.iter().any(|r| r["model"] == "test-model"));
    let rows = oct_agent::db::runs::list_subagent_runs(&app.pool, &conv)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].context_window, Some(12345));
    let sealed = rows[0].agent_config_json.as_ref().unwrap().clone();
    patch(
        &app,
        "/api/settings/agents/worker",
        json!({"instructions":"NEW-STYLE"}),
    )
    .await;
    assert_eq!(
        oct_agent::db::runs::get_run(&app.pool, &rows[0].id)
            .await
            .unwrap()
            .unwrap()
            .agent_config_json
            .as_deref(),
        Some(sealed.as_str())
    );
    let tasks: Value = app
        .get_json(&format!("/api/conversations/{conv}/subagents"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        tasks[0]["agent_config"]["tools"].as_array().unwrap().len(),
        2
    );
}

#[tokio::test]
async fn title_uses_own_project_model_and_falls_back_if_configuration_breaks() {
    let llm = spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("chat done"),
        finish_chunk("stop"),
    ])])
    .await;
    llm.push_title_turns([TitleTurn::Text("Configured title".into())]);
    let app = TestApp::new().await;
    app.register_llm_provider(&llm).await;
    add_model(&app, "small-title", false).await;
    let project = app.create_project().await;
    patch(
        &app,
        "/api/settings/agents/title",
        json!({"instructions":"TITLE-STYLE"}),
    )
    .await;
    patch(
        &app,
        &format!("/api/projects/{project}/settings/agents/title"),
        json!({"model":{"mode":"fixed","provider_id":"mock-llm","model_id":"small-title"}}),
    )
    .await;
    let create = app
        .post_json(
            &format!("/api/projects/{project}/conversations"),
            json!({"title":null}),
        )
        .await;
    let conv = create.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    collect_events(app.send_message(&conv, "title seed").await).await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let row = oct_agent::db::conversations::get_conversation(&app.pool, &conv)
            .await
            .unwrap()
            .unwrap();
        if row.title_source == "ai" {
            assert_eq!(row.title, "Configured title");
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
    let requests = llm.requests();
    let title = requests.iter().find(|r| r["stream"] != true).unwrap();
    assert_eq!(title["model"], "small-title");
    assert_eq!(title["max_tokens"], 100);
    assert!(title["tools"].is_null() || title["tools"] == json!([]));
    assert!(
        title["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("TITLE-STYLE")
    );
    // A stale configured reference must not become a second main-model call.
    oct_agent::db::models::delete_model(&app.pool, "mock-llm", "small-title")
        .await
        .unwrap();
    llm.push_turns([LlmTurn::Complete(vec![
        text_chunk("second chat done"),
        finish_chunk("stop"),
    ])]);
    let created = app
        .post_json(
            &format!("/api/projects/{project}/conversations"),
            json!({"title":null}),
        )
        .await;
    let next = created.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    collect_events(app.send_message(&next, "fallback seed").await).await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let row = oct_agent::db::conversations::get_conversation(&app.pool, &next)
            .await
            .unwrap()
            .unwrap();
        if row.title_source == "ai" {
            assert_eq!(row.title, "fallback seed");
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
    assert_eq!(
        llm.requests()
            .iter()
            .filter(|r| r["stream"] != true)
            .count(),
        1
    );
}

#[tokio::test]
async fn simultaneous_same_call_joins_one_sealed_worker() {
    let args = json!({"description":"same call","prompt":"JOIN-WORKER"}).to_string();
    let llm = spawn_llm_mock(vec![
        LlmTurn::Complete(vec![
            support::llm::tool_call_chunk_at(0, "same-id", "task", &args),
            support::llm::tool_call_chunk_at(1, "same-id", "task", &args),
            finish_chunk("tool_calls"),
        ]),
        LlmTurn::Complete(vec![text_chunk("done"), finish_chunk("stop")]),
    ])
    .await;
    llm.push_marker_turns(
        "JOIN-WORKER",
        [LlmTurn::Complete(vec![
            text_chunk("joined result"),
            finish_chunk("stop"),
        ])],
    );
    let app = TestApp::new().await;
    app.register_llm_provider(&llm).await;
    let project = app.create_project().await;
    let conv = app.create_conversation(&project).await;
    patch(
        &app,
        "/api/settings/agents/worker",
        json!({"instructions":"SEALED-JOIN", "tools":{"mode":"allowlist","items":[]}}),
    )
    .await;
    let events = collect_events(app.send_message(&conv, "delegate twice").await).await;
    let results: Vec<_> = events
        .iter()
        .filter(|e| e["type"] == "tool_result" && e["data"]["call_id"] == "same-id")
        .collect();
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|r| r["data"]["is_error"] == false));
    let rows = oct_agent::db::runs::list_subagent_runs(&app.pool, &conv)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "completed");
    assert_eq!(rows[0].execution_epoch, 1);
    assert_eq!(
        llm.requests()
            .iter()
            .filter(|r| r["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["role"] == "user"
                    && m["content"]
                        .as_str()
                        .is_some_and(|c| c.contains("JOIN-WORKER"))))
            .count(),
        1
    );
}
