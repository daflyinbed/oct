//! Catalog, scoped settings and full chat dispatch through a local MCP mock.
mod support;
use oct_agent::codemode::catalog::{Catalog, render};
use oct_mcp::McpCallResult;
use serde_json::{Value, json};
use std::sync::Arc;
use support::TestApp;
use support::mcp::{FakeMcpConnection, fake_tool};

async fn install(app: &TestApp, project: &str, hang: bool) -> (Arc<FakeMcpConnection>, Catalog) {
    let result = (!hang).then(|| McpCallResult {
        text: format!("PRIVATE_INTERMEDIATE:{}", "x".repeat(60000)),
        structured_json: Some(json!({"count":42,"blob":"y".repeat(60000)})),
        images: vec![],
        is_error: false,
    });
    install_with_result(app, project, result).await
}

async fn install_with_result(
    app: &TestApp,
    project: &str,
    result: Option<McpCallResult>,
) -> (Arc<FakeMcpConnection>, Catalog) {
    let row:Value=app.post_json("/api/mcp/servers",json!({"name":"test-api","transport":"stdio","command":"not-spawned","project_id":project})).await.json().await.unwrap();
    let mut info = fake_tool("search", Some("PRIVATE_MCP_TOOL_DESCRIPTION"));
    info.input_schema = json!({"type":"object","properties":{"a":{"type":"number"}},"required":["a"],"additionalProperties":false});
    info.output_schema = Some(
        json!({"type":"object","properties":{"count":{"type":"number"}},"required":["count"]}),
    );
    let connection = FakeMcpConnection::new(vec![info.clone()]);
    let connection = Arc::new(match result {
        Some(result) => connection.with_result("search", result),
        None => connection.with_hang("search"),
    });
    app.mcp.insert_connection_for_test(
        row["id"].as_str().unwrap(),
        "test-api",
        Some(project),
        connection.clone(),
        vec![info],
    );
    let tools: Vec<Arc<dyn oct_agent::tools::AgentTool>> = app
        .mcp
        .tools_for_run(Some(project))
        .into_iter()
        .map(Arc::from)
        .collect();
    (connection, Catalog::build(&tools).unwrap())
}

#[tokio::test]
async fn nested_mcp_errors_share_the_direct_structured_content_fallback() {
    use oct_agent::agent::EventHub;
    use oct_agent::permissions::gate::PermissionGate;
    use oct_agent::tool_host::dispatch;
    use oct_agent::tools::ToolContext;
    use std::sync::atomic::AtomicUsize;
    use tokio_util::sync::CancellationToken;

    for (text, structured) in [
        ("", Some(json!({"reason":"structured failure","code":409}))),
        (
            "text failure",
            Some(json!({"reason":"secondary diagnostics"})),
        ),
        ("", None),
    ] {
        let structured_only = text.is_empty() && structured.is_some();
        let app = TestApp::new().await;
        let project = app.create_project().await;
        let (_, catalog) = install_with_result(
            &app,
            &project,
            Some(McpCallResult {
                text: text.into(),
                structured_json: structured,
                images: vec![],
                is_error: true,
            }),
        )
        .await;
        let tool = catalog.tools.values().next().unwrap();
        let hub = Arc::new(EventHub::new());
        let ctx = ToolContext::new("nested-error", hub.clone(), CancellationToken::new());
        let gate = PermissionGate::permissive();
        let direct =
            dispatch::direct(Some(tool), tool.name(), json!({"a":1}), &ctx, &gate, &hub).await;
        assert!(direct.is_error);
        let nested = dispatch::nested(
            tool,
            json!({"a":1}),
            &ctx,
            &gate,
            &hub,
            &AtomicUsize::new(0),
        )
        .await
        .unwrap_err();
        assert_eq!(
            nested.to_string(),
            format!("MCP tool failed: {}", direct.content)
        );
        if structured_only {
            assert!(nested.to_string().contains("structured failure"));
            assert!(nested.to_string().contains("409"));
        }
    }
}

#[tokio::test]
async fn settings_inherit_override_and_restore_without_affecting_wire_log() {
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let path = format!("/api/projects/{project}/settings/mcp");
    let initial: Value = app.get_json(&path).await.json().await.unwrap();
    assert_eq!(initial["effective_mcp_tool_mode"], "direct");
    assert!(initial["mcp_tool_mode"].is_null());
    assert_eq!(
        app.put_json("/api/settings", json!({"mcp_tool_mode":"code"}))
            .await
            .status(),
        200
    );
    assert_eq!(
        app.get_json(&path).await.json::<Value>().await.unwrap()["effective_mcp_tool_mode"],
        "code"
    );
    let local: Value = app
        .put_json(&path, json!({"mcp_tool_mode":"direct"}))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(local["effective_mcp_tool_mode"], "direct");
    let inherited: Value = app
        .put_json(&path, json!({"mcp_tool_mode":null}))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(inherited["effective_mcp_tool_mode"], "code");
    assert!(inherited["mcp_tool_mode"].is_null());
    assert_eq!(
        app.put_json(&path, json!({"mcp_tool_mode":"invalid"}))
            .await
            .status(),
        422
    );
    assert_eq!(
        app.get_json("/api/projects/missing/settings/mcp")
            .await
            .status(),
        404
    );
}

#[tokio::test]
async fn unset_mode_defaults_to_code_until_direct_is_persisted() {
    // TestApp pins direct for determinism, so the app-level default needs a
    // raw pool with no settings row.
    let dir = tempfile::tempdir().unwrap();
    let pool = oct_agent::db::init_pool(&format!(
        "sqlite://{}",
        dir.path().join("default.db").display()
    ))
    .await
    .unwrap();
    assert_eq!(
        oct_agent::codemode::mode(&pool, None).await.unwrap(),
        oct_agent::codemode::McpToolMode::Code
    );
    oct_agent::db::settings::set(
        &pool,
        oct_agent::codemode::SETTING,
        oct_agent::codemode::McpToolMode::Direct.as_str(),
    )
    .await
    .unwrap();
    assert_eq!(
        oct_agent::codemode::mode(&pool, None).await.unwrap(),
        oct_agent::codemode::McpToolMode::Direct
    );
    // A project override beats both the global row and the built-in default.
    sqlx::query("INSERT INTO projects (id, name, working_dir) VALUES ('any-project', 'p', '/tmp')")
        .execute(&pool)
        .await
        .unwrap();
    oct_agent::codemode::set_project_mode(
        &pool,
        "any-project",
        Some(oct_agent::codemode::McpToolMode::Code),
    )
    .await
    .unwrap();
    assert_eq!(
        oct_agent::codemode::mode(&pool, Some("any-project"))
            .await
            .unwrap(),
        oct_agent::codemode::McpToolMode::Code
    );
}

#[tokio::test]
async fn code_mode_worker_allowlist_requires_read_tools_at_the_config_api() {
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let worker = format!("/api/projects/{project}/settings/agents/worker");
    // The TestApp pins direct mode: everything is storable until code mode
    // is switched on.
    assert_eq!(
        app.patch_json(
            &worker,
            json!({"tools":{"mode":"allowlist","items":[{"kind":"builtin","name":"write_file"}]}})
        )
        .await
        .status(),
        200
    );
    app.put_json("/api/settings", json!({"mcp_tool_mode":"code"}))
        .await;
    let response = app
        .patch_json(
            &worker,
            json!({"tools":{"mode":"allowlist","items":[{"kind":"builtin","name":"write_file"}]}}),
        )
        .await;
    assert_eq!(response.status(), 400);
    assert!(
        response
            .json::<Value>()
            .await
            .unwrap()
            .to_string()
            .contains("read_file")
    );
    // With the trio the same list is accepted; empty allowlists and the
    // parent mode stay valid (no code surface / nothing sealed).
    assert_eq!(
        app.patch_json(
            &worker,
            json!({"tools":{"mode":"allowlist","items":[
                {"kind":"builtin","name":"read_file"},
                {"kind":"builtin","name":"grep"},
                {"kind":"builtin","name":"glob"},
                {"kind":"builtin","name":"write_file"}
            ]}})
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        app.patch_json(&worker, json!({"tools":{"mode":"allowlist","items":[]}}))
            .await
            .status(),
        200
    );
    assert_eq!(
        app.patch_json(&worker, json!({"tools":{"mode":"parent"}}))
            .await
            .status(),
        200
    );
    // A project code override enforces the same rule even when the global
    // mode is direct.
    assert_eq!(
        app.put_json(
            &format!("/api/projects/{project}/settings/mcp"),
            json!({"mcp_tool_mode":"code"})
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        app.patch_json(
            &worker,
            json!({"tools":{"mode":"allowlist","items":[{"kind":"builtin","name":"write_file"}]}})
        )
        .await
        .status(),
        400
    );
}

#[tokio::test]
async fn code_mode_worker_without_read_tools_fails_at_spawn_after_mode_flip() {
    use support::llm::{LlmTurn, finish_chunk, spawn_llm_mock, text_chunk, tool_call_chunk};

    let app = TestApp::new().await;
    let project = app.create_project().await;
    // Sealed while the effective mode was direct, then the project flips to
    // code: the spawn guard must fail the run instead of handing the worker
    // a blind execute_code.
    assert_eq!(
        app.patch_json(
            &format!("/api/projects/{project}/settings/agents/worker"),
            json!({"tools":{"mode":"allowlist","items":[{"kind":"builtin","name":"write_file"}]}})
        )
        .await
        .status(),
        200
    );
    let llm = spawn_llm_mock(vec![
        LlmTurn::Complete(vec![
            tool_call_chunk(
                "task1",
                "task",
                &json!({"description":"blind","prompt":"BLIND-CODE-WORKER"}).to_string(),
            ),
            finish_chunk("tool_calls"),
        ]),
        LlmTurn::Complete(vec![text_chunk("parent done"), finish_chunk("stop")]),
    ])
    .await;
    app.register_llm_provider(&llm).await;
    app.put_json("/api/settings", json!({"mcp_tool_mode":"code"}))
        .await;
    let conv = app.create_conversation(&project).await;
    let response = app.send_message(&conv, "run").await;
    assert_eq!(response.status(), 200);
    app.read_sse(response).await;
    let stored = oct_agent::db::messages::list_messages(&app.pool, &conv)
        .await
        .unwrap();
    let task_result = stored
        .iter()
        .find(|row| row.role == "tool" && row.parts_json.contains("task1"))
        .expect("task result row");
    assert!(task_result.parts_json.contains("SDK discovery"));
    let runs = oct_agent::db::runs::list_subagent_runs(&app.pool, &conv)
        .await
        .unwrap();
    assert_eq!(runs[0].status, "failed");
    assert!(runs[0].error.as_deref().unwrap().contains("grep"));
}

#[tokio::test]
async fn declarations_bind_original_identity_and_publish_only_types() {
    let app = TestApp::new().await;
    let project = app.create_project().await;
    let (_, catalog) = install(&app, &project, false).await;
    let dir = tempfile::tempdir().unwrap();
    catalog.publish(&dir.path().join("sdk")).await.unwrap();
    let index: Value = serde_json::from_str(&catalog.files["index.json"]).unwrap();
    let path = index[0]["declarations"].as_str().unwrap();
    let text = std::fs::read_to_string(dir.path().join("sdk").join(path)).unwrap();
    assert!(text.contains("export declare function tool_search"));
    assert!(text.contains("\"a\": number"));
    assert!(text.contains("\"count\": number"));
    assert!(
        !dir.path()
            .join("sdk")
            .join(index[0]["importSpecifier"].as_str().unwrap())
            .exists()
    );
    assert_eq!(catalog.tools.len(), 1);
    assert!(render(&json!({"$defs":{"self":{"anyOf":[{"$ref":"#/$defs/self"},{"$ref":"#/$defs/self"}]}},"$ref":"#/$defs/self"})).len() < 16384);
    assert!(render(&json!({"type":"object","properties":{"bad*/\n":{"type":"string","description":"*/ inject\n"}},"additionalProperties":false})).contains("* / inject"));
}

fn fixture_tools(source: &str) -> Vec<Arc<dyn oct_agent::tools::AgentTool>> {
    let fixture: Value = serde_json::from_str(source).unwrap();
    let server = fixture["server"].as_str().unwrap();
    let infos: Vec<_> = fixture["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| oct_mcp::McpToolInfo {
            name: t["name"].as_str().unwrap().into(),
            description: t["description"].as_str().map(str::to_owned),
            input_schema: t["inputSchema"].clone(),
            output_schema: t.get("outputSchema").filter(|v| !v.is_null()).cloned(),
        })
        .collect();
    let connection = Arc::new(FakeMcpConnection::new(infos.clone()));
    infos
        .into_iter()
        .map(|info| {
            Arc::new(
                oct_agent::mcp::tool::McpTool::new(
                    oct_mcp::qualified_tool_name(server, &info.name),
                    info,
                    server.into(),
                    connection.clone(),
                    std::time::Duration::from_secs(1),
                )
                .with_server_id(format!("fixture:{server}")),
            ) as Arc<dyn oct_agent::tools::AgentTool>
        })
        .collect()
}

#[tokio::test]
async fn real_large_catalogs_are_searchable_and_cache_never_retains_run_tools() {
    let chrome = include_str!("fixtures/codemode/chrome-devtools.json");
    let ida = include_str!("fixtures/codemode/ida-pro.json");
    let mut tools = fixture_tools(chrome);
    tools.extend(fixture_tools(ida));
    let catalog = Catalog::build(&tools).unwrap();
    assert_eq!(catalog.tools.len(), 94);
    let index: Value = serde_json::from_str(&catalog.files["index.json"]).unwrap();
    for server in index.as_array().unwrap() {
        assert_eq!(server["split"], true);
        let entries: Value =
            serde_json::from_str(&catalog.files[server["toolIndex"].as_str().unwrap()]).unwrap();
        let discovery = &catalog.files[server["declarations"].as_str().unwrap()];
        for tool in entries.as_array().unwrap() {
            let alias = tool["export"].as_str().unwrap();
            assert!(discovery.contains(alias));
            let declaration = &catalog.files[tool["declarations"].as_str().unwrap()];
            assert!(declaration.contains(&format!("export declare function {alias}")));
            assert!(!declaration.contains("callTool("));
            assert!(
                declaration.lines().all(|line| line.chars().count() < 2000),
                "{}",
                tool["name"]
            );
            let schema: Value =
                serde_json::from_str(&catalog.files[tool["schema"].as_str().unwrap()]).unwrap();
            assert_eq!(schema["name"], tool["name"]);
            assert!(
                catalog.modules[server["importSpecifier"]
                    .as_str()
                    .unwrap()
                    .trim_start_matches("./")]
                .contains(&format!("export const {alias} ="))
            );
        }
    }
    let mut fresh = fixture_tools(chrome);
    fresh.extend(fixture_tools(ida));
    fresh.reverse();
    let cached = Catalog::build(&fresh).unwrap();
    assert_eq!(catalog.fingerprint, cached.fingerprint);
    assert!(Arc::ptr_eq(&catalog.files, &cached.files));
    for tool in fresh {
        let key = serde_json::to_string(&tool.selector()).unwrap();
        assert!(Arc::ptr_eq(&cached.tools[&key], &tool));
        assert!(!Arc::ptr_eq(&catalog.tools[&key], &tool));
    }
    let restricted: Vec<_> = cached.tools.values().take(1).cloned().collect();
    let partial = Catalog::build(&restricted).unwrap();
    assert_ne!(partial.fingerprint, catalog.fingerprint);
    assert_eq!(partial.tools.len(), 1);
    let dir = tempfile::tempdir().unwrap();
    catalog.publish(&dir.path().join("sdk")).await.unwrap();
    assert!(dir.path().join("sdk/catalog.json").exists());
}

#[tokio::test]
async fn crash_repair_uses_scoped_ledger_without_replaying_source() {
    use oct_agent::db::{messages, runs};
    use oct_llm_provider::core::{ContentPart, Message, Role, ToolCall};
    use support::llm::{LlmTurn, finish_chunk, spawn_llm_mock, text_chunk};
    for resume in [true, false] {
        let app = TestApp::new().await;
        let project = app.create_project().await;
        let conv = app.create_conversation(&project).await;
        let llm = spawn_llm_mock(vec![LlmTurn::Complete(vec![
            text_chunk("recovered"),
            finish_chunk("stop"),
        ])])
        .await;
        app.register_llm_provider(&llm).await;
        let run = runs::create_run(
            &app.pool,
            runs::NewRun {
                conversation_id: conv.clone(),
                kind: "root",
                parent_run_id: None,
                root_run_id: None,
                parent_tool_call_id: None,
                provider_id: Some("mock-llm".into()),
                model_id: Some("test-model".into()),
                context_window: None,
                description: None,
            },
        )
        .await
        .unwrap();
        let code = "text('kept'); await Promise.all([first(),second()]);";
        messages::insert_message(
            &app.pool,
            &conv,
            &Message {
                role: Role::Assistant,
                parts: vec![
                    ContentPart::ToolCall(ToolCall {
                        id: "crashed-code".into(),
                        name: "execute_code".into(),
                        arguments: json!({"code":code}).to_string(),
                    }),
                    ContentPart::ToolCall(ToolCall {
                        id: "native".into(),
                        name: "read_file".into(),
                        arguments: json!({"path":"README.md"}).to_string(),
                    }),
                ],
            },
            Some("mock-llm"),
            Some("test-model"),
            None,
        )
        .await
        .unwrap();
        runs::mark_stale_runs_lost(&app.pool).await.unwrap();
        let cell = app
            .dir
            .path()
            .join("codemode")
            .join(&conv)
            .join(&run.id)
            .join("cells")
            .join(oct_agent::codemode::catalog::digest("crashed-code"));
        std::fs::create_dir_all(&cell).unwrap();
        std::fs::write(cell.join("main.ts"), code).unwrap();
        std::fs::write(cell.join("output.log"), "kept\n").unwrap();
        let first = json!({"id":"crashed-code/1","name":"mcp__test__first","title":"First","status":"completed","error":null,"started_at_ms":1,"duration_ms":12});
        let pending = json!({"id":"crashed-code/2","name":"mcp__test__second","title":"Second","status":"running","error":null});
        let foreign = json!({"id":"other-cell/1","name":"mcp__test__forged","title":"Foreign","status":"completed","error":null});
        std::fs::write(
            cell.join("calls.jsonl"),
            format!("{first}\n{pending}\n{foreign}\n{{\"id\":"),
        )
        .unwrap();
        let response = if resume {
            app.resume(&conv).await
        } else {
            app.send_message(&conv, "continue").await
        };
        assert_eq!(response.status(), 200);
        app.read_sse(response).await;
        let stored = messages::list_messages(&app.pool, &conv).await.unwrap();
        let result = stored
            .iter()
            .find(|row| row.role == "tool" && row.parts_json.contains("crashed-code"))
            .unwrap();
        let details: Value = serde_json::from_str(result.details_json.as_ref().unwrap()).unwrap();
        assert_eq!(details["error_code"], "backend_restarted");
        assert_eq!(details["calls"].as_array().unwrap().len(), 2);
        assert_eq!(details["calls"][0]["status"], "completed");
        assert_eq!(details["calls"][1]["status"], "outcome_unknown");
        assert!(result.parts_json.contains("kept"));
        assert_eq!(stored.iter().filter(|r| r.role == "tool").count(), 2);
        assert_eq!(
            llm.requests()
                .iter()
                .filter(|r| r["stream"] == true)
                .count(),
            1
        );
        assert_eq!(app.resume(&conv).await.status(), 409);
    }
}

#[cfg(feature = "code-mode-tests")]
mod runtime {
    use super::*;
    use futures_util::StreamExt;
    use support::llm::{LlmTurn, finish_chunk, spawn_llm_mock, text_chunk, tool_call_chunk};

    fn code(catalog: &Catalog, body: &str) -> String {
        let index: Value = serde_json::from_str(&catalog.files["index.json"]).unwrap();
        format!(
            "import {{tool_search}} from {}; {body}",
            index[0]["importSpecifier"]
        )
    }
    fn turn(id: &str, name: &str, args: Value) -> LlmTurn {
        LlmTurn::Complete(vec![
            tool_call_chunk(id, name, &args.to_string()),
            finish_chunk("tool_calls"),
        ])
    }
    async fn setup(app: &TestApp, project: &str, code: String) -> (support::llm::LlmMock, String) {
        assert!(
            oct_codemode::worker_path().is_some(),
            "Build oct-codemode-worker with --features runtime before enabling code-mode-tests"
        );
        let llm = spawn_llm_mock(vec![
            turn("code1", "execute_code", json!({"code":code})),
            LlmTurn::Complete(vec![text_chunk("done"), finish_chunk("stop")]),
        ])
        .await;
        app.register_llm_provider(&llm).await;
        app.put_json("/api/settings", json!({"mcp_tool_mode":"code"}))
            .await;
        let conv = app.create_conversation(project).await;
        (llm, conv)
    }
    async fn events(app: &TestApp, conv: &str) -> Vec<Value> {
        app.read_sse(app.send_message(conv, "run code").await).await
    }
    fn result<'a>(events: &'a [Value], id: &str) -> &'a Value {
        &events
            .iter()
            .find(|e| e["type"] == "tool_result" && e["data"]["call_id"] == id)
            .expect("tool result")["data"]
    }

    #[tokio::test]
    async fn remote_timeout_keeps_the_outcome_unknown_instead_of_claiming_failure() {
        use oct_agent::tools::ToolContext;
        let app = TestApp::new().await;
        let project = app.create_project().await;
        let (connection, _) = install(&app, &project, true).await;
        let mut info = fake_tool("search", None);
        info.input_schema =
            json!({"type":"object","properties":{"a":{"type":"number"}},"required":["a"]});
        let tools: Vec<Arc<dyn oct_agent::tools::AgentTool>> = vec![Arc::new(
            oct_agent::mcp::tool::McpTool::new(
                "mcp__timeout__search".into(),
                info,
                "timeout".into(),
                connection.clone(),
                std::time::Duration::from_millis(10),
            )
            .with_server_id("timeout-server".into()),
        )];
        let catalog = Catalog::build(&tools).unwrap();
        let source = code(&catalog, "await tool_search({a:1});");
        let prepared = oct_agent::codemode::Prepared::new(
            app.dir.path().join("codemode"),
            "timeout-conv",
            "timeout-run",
            &tools,
        )
        .await
        .unwrap();
        let tool = prepared.tool(
            oct_agent::permissions::gate::PermissionGate::permissive(),
            oct_agent::tool_host::CodeHostEvents::new(Arc::new(oct_agent::agent::EventHub::new())),
        );
        let ctx = ToolContext::new(
            "timeout",
            Arc::new(oct_agent::agent::EventHub::new()),
            tokio_util::sync::CancellationToken::new(),
        );
        let output = tool.execute(json!({"code":source}), &ctx).await.unwrap();
        assert!(output.is_error);
        assert_eq!(connection.calls().len(), 1);
        let details = output.details.unwrap();
        assert_eq!(details["calls"][0]["status"], "outcome_unknown");
        assert_eq!(details["calls"][0]["error_code"], "tool_timeout");
    }

    #[tokio::test]
    async fn structured_mcp_error_reaches_script_and_persisted_call_diagnostics() {
        let app = TestApp::new().await;
        let project = app.create_project().await;
        let (connection, catalog) = install_with_result(
            &app,
            &project,
            Some(McpCallResult {
                text: String::new(),
                structured_json: Some(json!({"reason":"structured failure","code":409})),
                images: vec![],
                is_error: true,
            }),
        )
        .await;
        let (_, conv) = setup(
            &app,
            &project,
            code(
                &catalog,
                "try { await tool_search({a:1}); } catch (e) { text(e.name); text(e.message); text(e.code); text(e.result.structuredContent.code); }",
            ),
        )
        .await;
        app.set_permission_mode(&project, &conv, "bypass").await;
        let events = events(&app, &conv).await;
        assert_eq!(connection.calls().len(), 1);
        let output = result(&events, "code1");
        assert_eq!(output["is_error"], false, "{events:?}");
        let content = output["content"].as_str().unwrap();
        assert!(
            content.contains("ToolCallError\nMCP tool failed:"),
            "{content}"
        );
        assert!(content.contains("structured failure"), "{content}");
        assert!(content.contains("tool_error\n409\n"), "{content}");
        let persisted: Value = app
            .get_json(&format!("/api/conversations/{conv}/messages"))
            .await
            .json()
            .await
            .unwrap();
        assert!(persisted.to_string().contains("structured failure"));
        let call = &output["details"]["calls"][0];
        assert_eq!(call["status"], "failed");
        assert_eq!(call["error_code"], "tool_error");
        assert!(call["duration_ms"].is_u64());
        assert!(call["started_at_ms"].as_u64().unwrap() > 0);
        assert!(
            call["error"]
                .as_str()
                .unwrap()
                .contains("structured failure")
        );
        let trace =
            std::fs::read_to_string(output["details"]["trace_file"].as_str().unwrap()).unwrap();
        assert!(trace.contains("structured failure"), "{trace}");
    }

    #[tokio::test]
    async fn only_outer_tool_and_filtered_output_reach_the_model() {
        let app = TestApp::new().await;
        let project = app.create_project().await;
        let (connection, catalog) = install(&app, &project, false).await;
        let (llm,conv)=setup(&app,&project,code(&catalog,"const r = await tool_search({a:1}); text(r.structuredContent.count); text(r.text.length);")).await;
        app.set_permission_mode(&project, &conv, "bypass").await;
        let events = events(&app, &conv).await;
        assert_eq!(connection.calls(), vec![("search".into(), json!({"a":1}))]);
        let output = result(&events, "code1");
        assert_eq!(output["is_error"], false, "{events:?}");
        assert!(output["content"].as_str().unwrap().contains("42\n60021\n"));
        assert_eq!(output["details"]["calls"][0]["status"], "completed");
        let requests: Vec<_> = llm
            .requests()
            .into_iter()
            .filter(|r| r["stream"] == true)
            .collect();
        assert_eq!(requests.len(), 2);
        let names: Vec<_> = requests[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"execute_code"));
        assert!(!names.iter().any(|n| n.starts_with("mcp__")));
        assert!(
            !requests[0]["messages"][0]
                .to_string()
                .contains("PRIVATE_MCP_TOOL_DESCRIPTION")
        );
        assert!(!requests[1].to_string().contains("PRIVATE_INTERMEDIATE"));
        let tool_messages: Vec<_> = requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "tool")
            .collect();
        assert_eq!(tool_messages.len(), 1);
        assert_eq!(tool_messages[0]["tool_call_id"], "code1");
        assert_eq!(
            events
                .iter()
                .filter(|e| e["type"] == "tool_call_start")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| e["type"] == "nested_tool_update")
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn invalid_arguments_and_denied_approval_never_reach_mcp() {
        let app = TestApp::new().await;
        let project = app.create_project().await;
        let (connection, catalog) = install(&app, &project, false).await;
        let (_,conv)=setup(&app,&project,code(&catalog,"for (const a of ['invalid', 1]) { try { await tool_search({a}); } catch (e) { text(e.name); } }")).await;
        let mut stream = support::sse_payloads(app.send_message(&conv, "deny code call").await);
        let mut events = vec![];
        while let Some(event) =
            tokio::time::timeout(std::time::Duration::from_secs(15), stream.next())
                .await
                .unwrap()
        {
            if event["type"] == "approval_requested" {
                assert_eq!(event["data"]["call_id"], "code1/2");
                let id = event["data"]["approval_id"].as_str().unwrap();
                assert_eq!(
                    app.post_json(
                        &format!("/api/conversations/{conv}/approvals/{id}"),
                        json!({"decision":"deny"})
                    )
                    .await
                    .status(),
                    200
                );
            }
            events.push(event);
        }
        assert!(connection.calls().is_empty());
        let output = result(&events, "code1");
        assert_eq!(output["is_error"], false);
        assert!(
            output["content"]
                .as_str()
                .unwrap()
                .contains("ToolCallError\nToolCallError")
        );
        assert_eq!(output["details"]["calls"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn cancellation_preserves_call_ledger_and_resume_does_not_repeat_code() {
        let app = TestApp::new().await;
        let project = app.create_project().await;
        let (connection, catalog) = install(&app, &project, true).await;
        let (llm, conv) = setup(
            &app,
            &project,
            code(&catalog, "text('before'); await tool_search({a:1});"),
        )
        .await;
        app.set_permission_mode(&project, &conv, "bypass").await;
        let response = app.send_message(&conv, "cancel code").await;
        tokio::time::timeout(
            std::time::Duration::from_secs(15),
            connection.wait_for_call(),
        )
        .await
        .unwrap();
        assert_eq!(app.cancel(&conv).await.status(), 200);
        let events = app.read_sse(response).await;
        assert!(events.iter().any(|e| e["type"] == "cancelled"));
        let artifact = app.dir.path().join("codemode").join(&conv);
        let run = std::fs::read_dir(&artifact)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let cell = run
            .join("cells")
            .join(oct_agent::codemode::catalog::digest("code1"));
        assert_eq!(
            std::fs::read_to_string(cell.join("output.log")).unwrap(),
            "before\n"
        );
        let ledger = std::fs::read_to_string(cell.join("calls.jsonl")).unwrap();
        assert!(
            ledger.contains("outcome_unknown") || ledger.contains("failed"),
            "{ledger}"
        );
        app.read_sse(app.resume(&conv).await).await;
        assert_eq!(connection.calls().len(), 1);
        assert!(
            llm.requests()
                .iter()
                .any(|r| r["stream"] == true && r["messages"].to_string().contains("code1"))
        );
        assert_eq!(
            app.http_delete(&format!("/api/projects/{project}/conversations/{conv}"))
                .await
                .status(),
            204
        );
        assert!(!artifact.exists());
    }

    #[tokio::test]
    async fn worker_inherits_code_mode_and_original_authorized_tools() {
        let app = TestApp::new().await;
        let project = app.create_project().await;
        let (connection, catalog) = install(&app, &project, false).await;
        let name = catalog.tools.values().next().unwrap().name().to_owned();
        let llm = spawn_llm_mock(vec![
            turn(
                "task1",
                "task",
                json!({"description":"worker","prompt":"CODEMODE-WORKER"}),
            ),
            LlmTurn::Complete(vec![text_chunk("parent done"), finish_chunk("stop")]),
        ])
        .await;
        llm.push_marker_turns(
            "CODEMODE-WORKER",
            [
                turn("child1", "execute_code", json!({"code":code(&catalog,"text((await tool_search({a:2})).structuredContent.count);")})),
                LlmTurn::Complete(vec![text_chunk("child done"), finish_chunk("stop")]),
            ],
        );
        app.register_llm_provider(&llm).await;
        app.put_json("/api/settings", json!({"mcp_tool_mode":"code"}))
            .await;
        let conv = app.create_conversation(&project).await;
        app.set_permission_mode(&project, &conv, "bypass").await;
        let events = events(&app, &conv).await;
        assert_eq!(result(&events, "task1")["is_error"], false, "{events:?}");
        assert_eq!(connection.calls(), vec![("search".into(), json!({"a":2}))]);
        let child = llm
            .requests()
            .into_iter()
            .find(|r| {
                r["stream"] == true
                    && r["messages"].as_array().unwrap().iter().any(|m| {
                        m["role"] == "user" && m["content"].to_string().contains("CODEMODE-WORKER")
                    })
            })
            .unwrap();
        assert!(
            child["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["function"]["name"] == "execute_code")
        );
        assert!(
            !child["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["function"]["name"] == name)
        );
        let runs = oct_agent::db::runs::list_subagent_runs(&app.pool, &conv)
            .await
            .unwrap();
        let snapshot: Value =
            serde_json::from_str(runs[0].agent_config_json.as_ref().unwrap()).unwrap();
        assert_eq!(snapshot["tool_mode"], "code");
        assert_eq!(
            snapshot["code_catalog_version"],
            oct_agent::codemode::catalog::VERSION
        );
    }

    #[tokio::test]
    async fn native_code_works_without_mcp_and_shares_read_grants_with_direct_calls() {
        let app = TestApp::new().await;
        let project = app.create_project().await;
        std::fs::write(app.dir.path().join("native.txt"), "PRIVATE_NATIVE 42\n").unwrap();
        let source = r#"
            import * as native from './builtin/tools.js';
            const read = await native.tool_read_file({path:'native.txt'});
            await native.tool_write_file({path:'created.txt',content:'seed'});
            await native.tool_read_file({path:'created.txt'});
            await native.tool_edit_file({file_path:'created.txt',old_string:'seed',new_string:'done'});
            await Promise.all([
                native.tool_glob({pattern:'*.txt'}),
                native.tool_grep({pattern:'PRIVATE_NATIVE',path:'native.txt'}),
                native.tool_list_dir({depth:1})
            ]);
            const command = await native.tool_execute_command({command:"printf 'bash-ok'"});
            text({found:read.text.includes('PRIVATE_NATIVE'),bash:command.text.includes('bash-ok')});
        "#;
        let llm = spawn_llm_mock(vec![
            turn("native-code", "execute_code", json!({"code":source})),
            turn("direct-edit", "edit_file", json!({"file_path":"native.txt","old_string":"PRIVATE_NATIVE","new_string":"changed"})),
            LlmTurn::Complete(vec![text_chunk("done"), finish_chunk("stop")]),
        ]).await;
        app.register_llm_provider(&llm).await;
        app.put_json("/api/settings", json!({"mcp_tool_mode":"code"}))
            .await;
        let conv = app.create_conversation(&project).await;
        app.set_permission_mode(&project, &conv, "bypass").await;
        let events = events(&app, &conv).await;
        let output = result(&events, "native-code");
        assert_eq!(output["is_error"], false, "{events:?}");
        assert_eq!(output["details"]["calls"].as_array().unwrap().len(), 8);
        assert!(
            output["content"]
                .as_str()
                .unwrap()
                .contains("{\"found\":true,\"bash\":true}")
        );
        assert_eq!(result(&events, "direct-edit")["is_error"], false);
        assert_eq!(
            std::fs::read_to_string(app.dir.path().join("created.txt")).unwrap(),
            "done"
        );
        assert_eq!(
            std::fs::read_to_string(app.dir.path().join("native.txt")).unwrap(),
            "changed 42\n"
        );
        let requests = llm.requests();
        assert!(
            !requests
                .iter()
                .filter(|r| r["stream"] == true)
                .any(|r| r["messages"].to_string().contains("PRIVATE_NATIVE 42"))
        );
    }

    #[tokio::test]
    async fn restricted_worker_discovers_only_its_sdk_and_cannot_call_native_tools() {
        let app = TestApp::new().await;
        let project = app.create_project().await;
        let (connection, catalog) = install(&app, &project, false).await;
        let selector = catalog.tools.values().next().unwrap().selector();
        // Code-mode workers must carry the read trio (enforced by the
        // settings API and the spawn guard); it is what browses their SDK.
        assert_eq!(
            app.patch_json(
                &format!("/api/projects/{project}/settings/agents/worker"),
                json!({"tools":{"mode":"allowlist","items":[
                    {"kind":"builtin","name":"read_file"},
                    {"kind":"builtin","name":"grep"},
                    {"kind":"builtin","name":"glob"},
                    selector
                ]}})
            )
            .await
            .status(),
            200
        );
        let llm = spawn_llm_mock(vec![
            turn(
                "task1",
                "task",
                json!({"description":"restricted","prompt":"RESTRICTED-CODE-WORKER"}),
            ),
            LlmTurn::Complete(vec![text_chunk("parent done"), finish_chunk("stop")]),
        ])
        .await;
        let denied = serde_json::to_string(&oct_agent::agent_settings::ToolSelector::Builtin {
            name: "write_file".into(),
        })
        .unwrap();
        let source = code(
            &catalog,
            &format!(
                "try {{ await globalThis.__oct_call({}, {{path:'canary.txt',content:'bad'}}); }} catch(e) {{ text(e.code); }} text((await tool_search({{a:2}})).structuredContent.count);",
                serde_json::to_string(&denied).unwrap()
            ),
        );
        llm.push_marker_turns(
            "RESTRICTED-CODE-WORKER",
            [
                turn("discovery1", "read_file", json!({"path":"index.json"})),
                turn("child-code", "execute_code", json!({"code":source})),
                LlmTurn::Complete(vec![text_chunk("child done"), finish_chunk("stop")]),
            ],
        );
        app.register_llm_provider(&llm).await;
        app.put_json("/api/settings", json!({"mcp_tool_mode":"code"}))
            .await;
        let conv = app.create_conversation(&project).await;
        app.set_permission_mode(&project, &conv, "bypass").await;
        let events = events(&app, &conv).await;
        assert_eq!(result(&events, "task1")["is_error"], false, "{events:?}");
        assert_eq!(connection.calls().len(), 1);
        assert!(!app.dir.path().join("canary.txt").exists());
        let child: Vec<_> = llm
            .requests()
            .into_iter()
            .filter(|r| {
                r["stream"] == true
                    && r["messages"].as_array().unwrap().iter().any(|m| {
                        m["role"] == "user"
                            && m["content"].to_string().contains("RESTRICTED-CODE-WORKER")
                    })
            })
            .collect();
        for request in &child {
            let mut names: Vec<_> = request["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["function"]["name"].as_str().unwrap())
                .collect();
            names.sort_unstable();
            assert_eq!(names, ["execute_code", "glob", "grep", "read_file"]);
        }
        let messages = child.last().unwrap()["messages"].to_string();
        assert!(messages.contains("tool_unavailable"));
        assert!(messages.contains("Code mode: MCP tools are not exposed"));
        assert!(messages.contains("Loaded SDK server groups"));
        let runs = oct_agent::db::runs::list_subagent_runs(&app.pool, &conv)
            .await
            .unwrap();
        let sdk = app
            .dir
            .path()
            .join("codemode")
            .join(&runs[0].conversation_id)
            .join(&runs[0].id)
            .join("sdk-1");
        let index: Value =
            serde_json::from_str(&std::fs::read_to_string(sdk.join("index.json")).unwrap())
                .unwrap();
        let kinds: Vec<&str> = index
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["kind"].as_str().unwrap())
            .collect();
        // Only the selected interfaces: the read trio (builtin) plus the one
        // allowed MCP server; write_file and friends never appear.
        assert_eq!(kinds, ["builtin", "mcp"]);
        let builtin = index
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["kind"] == "builtin")
            .unwrap();
        assert_eq!(builtin["tools"], 3);
    }

    #[tokio::test]
    async fn worker_native_code_has_independent_reads_and_keeps_background_disabled() {
        let app = TestApp::new().await;
        let project = app.create_project().await;
        std::fs::write(app.dir.path().join("shared.txt"), "before").unwrap();
        app.patch_json(&format!("/api/projects/{project}/settings/agents/worker"), json!({"tools":{"mode":"allowlist","items":[
            {"kind":"builtin","name":"read_file"},{"kind":"builtin","name":"grep"},{"kind":"builtin","name":"glob"},
            {"kind":"builtin","name":"edit_file"},{"kind":"builtin","name":"execute_command"}
        ]}})).await;
        let llm = spawn_llm_mock(vec![
            turn("parent-read", "read_file", json!({"path":"shared.txt"})),
            turn(
                "task1",
                "task",
                json!({"description":"native worker","prompt":"NATIVE-CODE-WORKER"}),
            ),
            LlmTurn::Complete(vec![text_chunk("parent done"), finish_chunk("stop")]),
        ])
        .await;
        llm.push_marker_turns("NATIVE-CODE-WORKER", [
            turn("child-code", "execute_code", json!({"code":r#"
                import * as native from './builtin/tools.js';
                try { await native.tool_edit_file({file_path:'shared.txt',old_string:'before',new_string:'bad'}); }
                catch(e) { text(e.result.text); }
                await native.tool_read_file({path:'shared.txt'});
                await native.tool_edit_file({file_path:'shared.txt',old_string:'before',new_string:'after'});
                try { await native.tool_execute_command({command:'cat',run_in_background:true}); }
                catch(e) { text(e.result.text); }
                text((await native.tool_execute_command({command:"printf 'worker-ok'"})).text);
            "#})),
            LlmTurn::Complete(vec![text_chunk("child done"), finish_chunk("stop")]),
        ]);
        app.register_llm_provider(&llm).await;
        app.put_json("/api/settings", json!({"mcp_tool_mode":"code"}))
            .await;
        let conv = app.create_conversation(&project).await;
        app.set_permission_mode(&project, &conv, "bypass").await;
        let events = events(&app, &conv).await;
        assert_eq!(result(&events, "task1")["is_error"], false, "{events:?}");
        assert_eq!(
            std::fs::read_to_string(app.dir.path().join("shared.txt")).unwrap(),
            "after"
        );
        let child = llm
            .requests()
            .into_iter()
            .rfind(|r| {
                r["stream"] == true
                    && r["messages"].as_array().unwrap().iter().any(|m| {
                        m["role"] == "user"
                            && m["content"].to_string().contains("NATIVE-CODE-WORKER")
                    })
            })
            .unwrap();
        let messages = child["messages"].to_string();
        assert!(
            messages.contains("File has not been read yet"),
            "{messages}"
        );
        assert!(messages.contains("run_in_background is not available"));
        assert!(messages.contains("worker-ok"));
        assert!(
            oct_agent::db::background::list_tasks_for_conversation(&app.pool, &conv)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn root_code_background_commands_are_owned_by_the_outer_timeline_call() {
        let app = TestApp::new().await;
        let project = app.create_project().await;
        let (llm, conv) = setup(
            &app,
            &project,
            r#"
            import {tool_execute_command,tool_task_stop} from './builtin/tools.js';
            const started = await tool_execute_command({command:'tail -f /dev/null',run_in_background:true});
            const id = started.text.match(/task_id: (bg_\S+)/)[1];
            text((await tool_task_stop({task_id:id})).text);
        "#
            .into(),
        )
        .await;
        llm.push_marker_turns(
            "<task-notification>",
            [LlmTurn::Complete(vec![
                text_chunk("notified"),
                finish_chunk("stop"),
            ])],
        );
        app.set_permission_mode(&project, &conv, "bypass").await;
        let events = events(&app, &conv).await;
        assert_eq!(result(&events, "code1")["is_error"], false, "{events:?}");
        let tasks: Value = app
            .get_json(&format!("/api/conversations/{conv}/background-tasks"))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(tasks.as_array().unwrap().len(), 1);
        let tasks = oct_agent::db::background::list_tasks_for_conversation(&app.pool, &conv)
            .await
            .unwrap();
        assert_eq!(tasks[0].parent_call_id, "code1");
        assert!(matches!(
            tasks[0].status.as_str(),
            "cancelled" | "completed"
        ));
    }

    #[tokio::test]
    async fn worker_code_write_uses_parent_approval_and_denial_has_no_side_effect() {
        let app = TestApp::new().await;
        let project = app.create_project().await;
        app.patch_json(
            &format!("/api/projects/{project}/settings/agents/worker"),
            json!({"tools":{"mode":"allowlist","items":[
                {"kind":"builtin","name":"read_file"},{"kind":"builtin","name":"grep"},{"kind":"builtin","name":"glob"},
                {"kind":"builtin","name":"write_file"}
            ]}}),
        )
        .await;
        let llm = spawn_llm_mock(vec![
            turn(
                "task1",
                "task",
                json!({"description":"approval","prompt":"CODE-WRITE-APPROVAL"}),
            ),
            LlmTurn::Complete(vec![text_chunk("parent done"), finish_chunk("stop")]),
        ])
        .await;
        llm.push_marker_turns(
            "CODE-WRITE-APPROVAL",
            [
                turn(
                    "child-code",
                    "execute_code",
                    json!({"code":r#"
                import {tool_write_file} from './builtin/tools.js';
                try { await tool_write_file({path:'denied.txt',content:'bad'}); }
                catch(e) { text(e.code); }
            "#}),
                ),
                LlmTurn::Complete(vec![text_chunk("child done"), finish_chunk("stop")]),
            ],
        );
        app.register_llm_provider(&llm).await;
        app.put_json("/api/settings", json!({"mcp_tool_mode":"code"}))
            .await;
        let conv = app.create_conversation(&project).await;
        let mut stream = support::sse_payloads(app.send_message(&conv, "approval").await);
        let mut events = Vec::new();
        let mut approved = 0;
        while let Some(event) =
            tokio::time::timeout(std::time::Duration::from_secs(15), stream.next())
                .await
                .unwrap()
        {
            if event["type"] == "approval_requested" {
                assert_eq!(event["data"]["call_id"], "child-code/1");
                assert_ne!(event["data"]["agent_id"].as_str(), Some(conv.as_str()));
                let id = event["data"]["approval_id"].as_str().unwrap();
                assert_eq!(
                    app.post_json(
                        &format!("/api/conversations/{conv}/approvals/{id}"),
                        json!({"decision":"deny"})
                    )
                    .await
                    .status(),
                    200
                );
                approved += 1;
            }
            events.push(event);
        }
        assert_eq!(approved, 1);
        assert_eq!(result(&events, "task1")["is_error"], false);
        assert!(!app.dir.path().join("denied.txt").exists());
        let child = llm
            .requests()
            .into_iter()
            .rfind(|r| {
                r["stream"] == true
                    && r["messages"].as_array().unwrap().iter().any(|m| {
                        m["role"] == "user"
                            && m["content"].to_string().contains("CODE-WRITE-APPROVAL")
                    })
            })
            .unwrap();
        assert!(child["messages"].to_string().contains("permission_denied"));
    }

    #[tokio::test]
    async fn code_worker_with_empty_allowlist_receives_no_discovery_or_execution_tools() {
        let app = TestApp::new().await;
        let project = app.create_project().await;
        app.patch_json(
            &format!("/api/projects/{project}/settings/agents/worker"),
            json!({"tools":{"mode":"allowlist","items":[]}}),
        )
        .await;
        let llm = spawn_llm_mock(vec![
            turn(
                "task1",
                "task",
                json!({"description":"empty","prompt":"EMPTY-CODE-WORKER"}),
            ),
            LlmTurn::Complete(vec![text_chunk("parent done"), finish_chunk("stop")]),
        ])
        .await;
        llm.push_marker_turns(
            "EMPTY-CODE-WORKER",
            [LlmTurn::Complete(vec![
                text_chunk("child done"),
                finish_chunk("stop"),
            ])],
        );
        app.register_llm_provider(&llm).await;
        app.put_json("/api/settings", json!({"mcp_tool_mode":"code"}))
            .await;
        let conv = app.create_conversation(&project).await;
        let events = events(&app, &conv).await;
        assert_eq!(result(&events, "task1")["is_error"], false);
        let requests = llm.requests();
        let child = requests
            .iter()
            .find(|request| {
                request["messages"].as_array().is_some_and(|messages| {
                    messages.iter().any(|message| {
                        message["role"] == "user"
                            && message["content"].to_string().contains("EMPTY-CODE-WORKER")
                    })
                })
            })
            .unwrap();
        assert!(
            child
                .get("tools")
                .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty))
        );
        let row = oct_agent::db::runs::list_subagent_runs(&app.pool, &conv)
            .await
            .unwrap()
            .remove(0);
        let snapshot =
            oct_agent::agent_settings::WorkerSnapshot::decode(row.agent_config_json.as_deref())
                .unwrap()
                .unwrap();
        assert_eq!(snapshot.tool_mode, oct_agent::codemode::McpToolMode::Code);
        assert!(snapshot.tools.is_empty());
        assert!(
            !app.dir
                .path()
                .join("codemode")
                .join(row.conversation_id)
                .exists()
        );
    }

    #[tokio::test]
    async fn crashed_worker_keeps_code_snapshot_and_recovers_trace_without_replaying() {
        use oct_agent::agent_settings::ToolSelector;
        let app = TestApp::new().await;
        let project = app.create_project().await;
        let (connection, catalog) = install(&app, &project, true).await;
        let selector = catalog.tools.values().next().unwrap().selector();
        app.patch_json(
            &format!("/api/projects/{project}/settings/agents/worker"),
            json!({"tools":{"mode":"allowlist","items":[
                {"kind":"builtin","name":"read_file"},
                {"kind":"builtin","name":"grep"},
                {"kind":"builtin","name":"glob"},
                selector
            ]}}),
        )
        .await;
        let llm = spawn_llm_mock(vec![turn(
            "task1",
            "task",
            json!({"description":"crash","prompt":"CRASHED-CODE-WORKER"}),
        )])
        .await;
        llm.push_marker_turns(
            "CRASHED-CODE-WORKER",
            [turn(
                "child-code",
                "execute_code",
                json!({"code":code(&catalog, "text('before-crash'); await tool_search({a:1});")}),
            )],
        );
        app.register_llm_provider(&llm).await;
        app.put_json("/api/settings", json!({"mcp_tool_mode":"code"}))
            .await;
        let conv = app.create_conversation(&project).await;
        app.set_permission_mode(&project, &conv, "bypass").await;
        let response = app.send_message(&conv, "crash").await;
        tokio::time::timeout(
            std::time::Duration::from_secs(15),
            connection.wait_for_call(),
        )
        .await
        .unwrap();
        let child = oct_agent::db::runs::list_subagent_runs(&app.pool, &conv)
            .await
            .unwrap()
            .remove(0);
        let sealed = child.agent_config_json.clone().unwrap();
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&app.pool)
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::copy(app.dir.path().join("test.db"), dir.path().join("test.db")).unwrap();
        fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
            std::fs::create_dir_all(to).unwrap();
            for item in std::fs::read_dir(from).unwrap() {
                let item = item.unwrap();
                let destination = to.join(item.file_name());
                if item.file_type().unwrap().is_dir() {
                    copy_tree(&item.path(), &destination);
                } else {
                    std::fs::copy(item.path(), destination).unwrap();
                }
            }
        }
        copy_tree(
            &app.dir.path().join("codemode"),
            &dir.path().join("codemode"),
        );
        let recovered = TestApp::new_with_dir(dir).await;
        app.cancel(&conv).await;
        app.read_sse(response).await;
        recovered
            .put_json("/api/settings", json!({"mcp_tool_mode":"direct"}))
            .await;
        recovered
            .patch_json(
                &format!("/api/projects/{project}/settings/agents/worker"),
                json!({"tools":{"mode":"allowlist","items":[]}}),
            )
            .await;
        let ToolSelector::Mcp { server_id, .. } = selector else {
            unreachable!()
        };
        let info = fake_tool("search", Some("PRIVATE_MCP_TOOL_DESCRIPTION"));
        let fresh = Arc::new(FakeMcpConnection::new(vec![info.clone()]).with_hang("search"));
        recovered.mcp.insert_connection_for_test(
            &server_id,
            "test-api",
            Some(project.as_str()),
            fresh.clone(),
            vec![info],
        );
        llm.push_marker_turns(
            "CRASHED-CODE-WORKER",
            [
                turn(
                    "recovered-code",
                    "execute_code",
                    json!({"code":"text('recovered');"}),
                ),
                LlmTurn::Complete(vec![text_chunk("child recovered"), finish_chunk("stop")]),
            ],
        );
        llm.push_turns([LlmTurn::Complete(vec![
            text_chunk("parent recovered"),
            finish_chunk("stop"),
        ])]);
        let events = recovered.read_sse(recovered.resume(&conv).await).await;
        // Recovery backfills the managed task directly into the timeline;
        // it does not replay the original live tool_result event.
        let history: Vec<Value> = recovered
            .get_json(&format!("/api/conversations/{conv}/messages"))
            .await
            .json()
            .await
            .unwrap();
        let task_result = history
            .iter()
            .flat_map(|message| message["parts"].as_array().unwrap())
            .find(|part| part["type"] == "tool_result" && part["call_id"] == "task1")
            .expect("persisted recovered task result");
        assert_eq!(task_result["is_error"], false, "{events:?} {history:?}");
        assert!(
            history
                .iter()
                .any(|message| message["parts"].to_string().contains("parent recovered"))
        );
        assert_eq!(connection.calls().len(), 1);
        assert!(
            fresh.calls().is_empty(),
            "the interrupted MCP call must not be replayed"
        );
        let row = oct_agent::db::runs::get_run(&recovered.pool, &child.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.execution_epoch, 2);
        assert_eq!(row.status, "completed");
        assert_eq!(row.agent_config_json.as_deref(), Some(sealed.as_str()));
        let messages =
            oct_agent::db::messages::list_messages(&recovered.pool, &child.conversation_id)
                .await
                .unwrap();
        let repaired = messages
            .iter()
            .find(|m| {
                m.details_json
                    .as_ref()
                    .is_some_and(|d| d.contains("backend_restarted"))
            })
            .expect("scoped crash diagnostics");
        assert!(repaired.parts_json.contains("before-crash"));
        assert!(
            repaired
                .details_json
                .as_ref()
                .unwrap()
                .contains("outcome_unknown")
        );
        let root = recovered
            .dir
            .path()
            .join("codemode")
            .join(&child.conversation_id)
            .join(&child.id);
        assert!(root.join("sdk-1/index.json").is_file());
        assert!(root.join("sdk-2/index.json").is_file());
        let requests = llm.requests();
        let resumed = requests
            .iter()
            .find(|r| {
                r["stream"] == true && r["messages"].to_string().contains("backend_restarted")
            })
            .unwrap();
        let mut names: Vec<_> = resumed["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["execute_code", "glob", "grep", "read_file"]);
    }
}
