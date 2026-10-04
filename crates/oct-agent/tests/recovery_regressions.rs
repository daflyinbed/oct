mod support;
use oct_agent::{agent::AgentEvent, db};
use oct_llm_provider::core::{FinishReason, StreamEvent, ToolCall};
use support::llm::{LlmTurn, finish_chunk, spawn_llm_mock, text_chunk, tool_call_chunk};
use support::{
    ScriptedModel, TestEnv, Turn, collect_events, is_terminal, new_conversation, spawn_agent,
};

#[tokio::test]
async fn eof_without_finish_is_failed() {
    let env = TestEnv::new().await;
    let conv = new_conversation(&env.pool, env.dir.path()).await;
    let model = ScriptedModel::new(vec![Turn::Stream(vec![Ok(StreamEvent::TextDelta(
        "partial".into(),
    ))])]);
    let (_handle, mut stream) = spawn_agent(model, vec![], env.pool.clone(), &conv, "hello").await;
    let events = collect_events(&mut stream, is_terminal).await;
    assert!(matches!(events.last(), Some(AgentEvent::Error(_))));
    assert_eq!(
        db::messages::list_messages(&env.pool, &conv)
            .await
            .unwrap()
            .len(),
        0
    );
}

#[tokio::test]
async fn assistant_insert_failure_prevents_side_effect() {
    let env = TestEnv::new().await;
    let conv = new_conversation(&env.pool, env.dir.path()).await;
    sqlx::query("CREATE TRIGGER audit_fail BEFORE INSERT ON messages WHEN NEW.role = 'assistant' BEGIN SELECT RAISE(FAIL, 'audit storage fault'); END")
        .execute(&env.pool).await.unwrap();
    let model = ScriptedModel::new(vec![
        Turn::Stream(vec![
            Ok(StreamEvent::ToolCall(ToolCall {
                id: "audit-call".into(),
                name: "write_file".into(),
                arguments: r#"{"path":"side-effect.txt","content":"executed"}"#.into(),
            })),
            Ok(StreamEvent::Finish(FinishReason::ToolCalls)),
        ]),
        Turn::Stream(vec![
            Ok(StreamEvent::TextDelta("done".into())),
            Ok(StreamEvent::Finish(FinishReason::Stop)),
        ]),
    ]);
    let roots =
        oct_agent::tools::workspace::WorkspaceRoots::new(env.dir.path().to_path_buf(), vec![]);
    let tools = oct_agent::tools::default_tools_with(
        roots,
        std::sync::Arc::new(oct_agent::tools::shared::WorkspaceFileLocks::new()),
        std::sync::Arc::new(oct_agent::tools::shared::AgentReadState::new()),
    );
    let (_handle, mut stream) = spawn_agent(model, tools, env.pool.clone(), &conv, "write").await;
    let events = collect_events(&mut stream, is_terminal).await;
    assert!(matches!(events.last(), Some(AgentEvent::Error(_))));
    assert!(!env.dir.path().join("side-effect.txt").exists());
    assert!(
        db::messages::list_messages(&env.pool, &conv)
            .await
            .unwrap()
            .iter()
            .all(|m| m.role != "assistant")
    );
}

#[tokio::test]
async fn failed_model_request_after_tools_can_resume() {
    let llm = spawn_llm_mock(vec![
        LlmTurn::Complete(vec![
            tool_call_chunk("call", "list_dir", r#"{"path":"."}"#),
            finish_chunk("tool_calls"),
        ]),
        LlmTurn::Fail(503),
    ])
    .await;
    let app = support::TestApp::new().await;
    app.register_llm_provider(&llm).await;
    let p = app.create_project().await;
    let c = app.create_conversation(&p).await;
    let events = app.read_sse(app.send_message(&c, "hello").await).await;
    assert!(events.iter().any(|e| e["type"] == "error"));
    assert_eq!(
        db::messages::last_message_role(&app.pool, &c)
            .await
            .unwrap()
            .as_deref(),
        Some("tool")
    );
    let resumed = app.resume(&c).await;
    assert_eq!(resumed.status(), 200);
    app.read_sse(resumed).await;
}

#[tokio::test]
async fn delete_conversation_stops_root() {
    let llm = spawn_llm_mock(vec![LlmTurn::Hang(vec![text_chunk("started")])]).await;
    let app = support::TestApp::new().await;
    app.register_llm_provider(&llm).await;
    let p = app.create_project().await;
    let c = app.create_conversation(&p).await;
    let response = app.send_message(&c, "hello").await;
    let mut stream = support::sse_payloads(response);
    use futures_util::StreamExt;
    tokio::time::timeout(std::time::Duration::from_secs(10), stream.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        app.http_delete(&format!("/api/projects/{p}/conversations/{c}"))
            .await
            .status(),
        204
    );
    let status: serde_json::Value = app.run_status(&c).await.json().await.unwrap();
    assert_eq!(status["running"], false);
    app.cancel(&c).await;
    while tokio::time::timeout(std::time::Duration::from_secs(10), stream.next())
        .await
        .unwrap()
        .is_some()
    {}
}

#[cfg(unix)]
#[tokio::test]
async fn cancel_during_registration_kills_unregistered_process() {
    let app = support::TestApp::new().await;
    let p = app.create_project().await;
    let c = app.create_conversation(&p).await;
    // Exhaust the pool: spawn succeeds, but task registration cannot start.
    let mut held = Vec::new();
    for _ in 0..5 {
        held.push(app.pool.acquire().await.unwrap());
    }
    let pid_file = app.dir.path().join("audit-pid");
    let manager = app.background.clone();
    let command = format!("echo $$ > '{}'; exec sleep 60", pid_file.display());
    let cwd = app.dir.path().to_path_buf();
    let join = tokio::spawn(async move {
        manager
            .start_bash(oct_agent::runtime::background::BashStart {
                conversation_id: c,
                parent_run_id: None,
                parent_call_id: "audit".into(),
                command,
                cwd,
            })
            .await
    });
    let pid = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(s) = tokio::fs::read_to_string(&pid_file).await {
                if let Ok(pid) = s.trim().parse::<i32>() {
                    break pid;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    join.abort();
    assert!(join.await.unwrap_err().is_cancelled());
    let alive = !wait_process_gone(pid).await;
    // Always clean up the test-owned process group before asserting.
    unsafe {
        libc::killpg(pid, libc::SIGKILL);
    }
    drop(held);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM background_tasks")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert!(!alive, "the dropped startup future must kill its process");
    assert_eq!(count, 0, "the orphan has no durable task identity");
}

#[cfg(unix)]
#[tokio::test]
async fn cancel_kills_term_ignoring_descendant() {
    let app = support::TestApp::new().await;
    let p = app.create_project().await;
    let c = app.create_conversation(&p).await;
    let pid_file = app.dir.path().join("audit-descendant-pid");
    let task = app
        .background
        .start_bash(oct_agent::runtime::background::BashStart {
            conversation_id: c,
            parent_run_id: None,
            parent_call_id: "audit".into(),
            command: format!(
                "sh -c 'trap \"\" TERM; echo $$ > \"{}\"; exec sleep 60' & wait",
                pid_file.display()
            ),
            cwd: app.dir.path().to_path_buf(),
        })
        .await
        .unwrap();
    let pid = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(s) = tokio::fs::read_to_string(&pid_file).await {
                if let Ok(pid) = s.trim().parse::<i32>() {
                    break pid;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let row = app.background.cancel_and_wait(&task.id).await.unwrap();
    let alive = !wait_process_gone(pid).await;
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    assert_eq!(row.status, "cancelled");
    assert!(!alive, "task was cancelled, but its descendant survived");
}

#[cfg(unix)]
async fn wait_process_gone(pid: i32) -> bool {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let path = format!("/proc/{pid}/stat");
            match tokio::fs::read_to_string(path).await {
                Err(_) => break,
                Ok(stat) if stat.split(") ").nth(1).is_some_and(|s| s.starts_with('Z')) => break,
                _ => tokio::task::yield_now().await,
            }
        }
    })
    .await
    .is_ok()
}

#[tokio::test]
async fn permission_storage_failure_refuses_to_start_model() {
    let llm = spawn_llm_mock(vec![LlmTurn::Complete(vec![
        text_chunk("must not run"),
        finish_chunk("stop"),
    ])])
    .await;
    let app = support::TestApp::new().await;
    app.register_llm_provider(&llm).await;
    let p = app.create_project().await;
    let c = app.create_conversation(&p).await;
    sqlx::query("DROP TABLE permission_rules")
        .execute(&app.pool)
        .await
        .unwrap();
    assert_eq!(app.send_message(&c, "hello").await.status(), 500);
    assert!(!llm.requests().iter().any(|r| r["stream"] == true));
    let run = db::runs::latest_run_for_conversation(&app.pool, &c)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run.status, "failed");
}

#[tokio::test]
async fn invalid_permission_patch_does_not_change_title() {
    let app = support::TestApp::new().await;
    let p = app.create_project().await;
    let c = app.create_conversation(&p).await;
    let before = db::conversations::get_conversation(&app.pool, &c)
        .await
        .unwrap()
        .unwrap();
    let response = app
        .patch_json(
            &format!("/api/projects/{p}/conversations/{c}"),
            serde_json::json!({"title":"changed", "permission_mode":"invalid"}),
        )
        .await;
    assert_eq!(response.status(), 400);
    let after = db::conversations::get_conversation(&app.pool, &c)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before.title, after.title);
}

#[tokio::test]
async fn cross_project_delete_is_rejected() {
    let app = support::TestApp::new().await;
    let p = app.create_project().await;
    let other = app.create_project().await;
    let c = app.create_conversation(&p).await;
    assert_eq!(
        app.http_delete(&format!("/api/projects/{other}/conversations/{c}"))
            .await
            .status(),
        404
    );
    assert!(
        db::conversations::get_conversation(&app.pool, &c)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn critical_result_retains_ownership_until_storage_recovers() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let env = TestEnv::new().await;
    let available = Arc::new(AtomicBool::new(false));
    let attempted = Arc::new(tokio::sync::Notify::new());
    let flag = available.clone();
    let signal = attempted.clone();
    let pool = env.pool.clone();
    let writer = tokio::spawn(async move {
        db::persist_critical(&pool, || async {
            signal.notify_one();
            anyhow::ensure!(flag.load(Ordering::SeqCst), "injected storage failure");
            Ok(42)
        })
        .await
    });
    attempted.notified().await;
    assert!(!writer.is_finished());
    available.store(true, Ordering::SeqCst);
    assert_eq!(writer.await.unwrap().unwrap(), 42);
}

#[tokio::test]
async fn delivered_notification_retains_wake_until_consuming_run_completes() {
    use oct_llm_provider::core::{Message, Role};
    let env = TestEnv::new().await;
    let conv = new_conversation(&env.pool, env.dir.path()).await;
    let msg = db::background::insert_notification_marking_tasks(
        &env.pool,
        &conv,
        &Message::text(Role::User, "notification"),
        None,
        &[],
    )
    .await
    .unwrap();
    assert_eq!(
        db::background::pending_wake(&env.pool, &conv)
            .await
            .unwrap(),
        Some(msg.clone())
    );
    let run = db::runs::create_run(
        &env.pool,
        db::runs::NewRun {
            conversation_id: conv.clone(),
            kind: "root",
            parent_run_id: None,
            root_run_id: None,
            parent_tool_call_id: None,
            provider_id: None,
            model_id: None,
            context_window: None,
            description: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        db::background::pending_wake(&env.pool, &conv)
            .await
            .unwrap(),
        Some(msg)
    );
    db::runs::finish_run(
        &env.pool,
        &run.id,
        db::runs::RunFinalStatus::Completed,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(
        db::background::pending_wake(&env.pool, &conv)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn failed_wake_runs_back_off_before_retrying_the_durable_intent() {
    use oct_llm_provider::core::{Message, Role};
    use std::time::Duration;

    for status in [401, 429, 503] {
        let llm = spawn_llm_mock(vec![
            LlmTurn::Complete(vec![text_chunk("ready"), finish_chunk("stop")]),
            LlmTurn::Fail(status),
            LlmTurn::Complete(vec![
                text_chunk("processed notification"),
                finish_chunk("stop"),
            ]),
        ])
        .await;
        let app = support::TestApp::new().await;
        app.register_llm_provider(&llm).await;
        let project = app.create_project().await;
        let conv = app.create_conversation(&project).await;
        app.read_sse(app.send_message(&conv, "hello").await).await;
        let message_id = db::background::insert_notification_marking_tasks(
            &app.pool,
            &conv,
            &Message::text(
                Role::User,
                "<task-notification>finished</task-notification>",
            ),
            None,
            &[],
        )
        .await
        .unwrap();
        app.background.kick_notification(&conv);

        let failed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let run = db::runs::latest_run_for_conversation(&app.pool, &conv)
                    .await
                    .unwrap()
                    .unwrap();
                if run.status == "failed" {
                    break run;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("wake run failed");

        // Observe a full two-second window after settlement: the old idle
        // polling loop would launch another request within one second.
        let premature_retry = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if llm
                    .requests()
                    .iter()
                    .filter(|request| request["stream"] == true)
                    .count()
                    > 2
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            premature_retry.is_err(),
            "HTTP {status}: wake retried without failure backoff"
        );
        assert_eq!(
            db::background::pending_wake(&app.pool, &conv)
                .await
                .unwrap(),
            Some(message_id.clone()),
            "failure must retain the wake intent"
        );

        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let run = db::runs::latest_run_for_conversation(&app.pool, &conv)
                    .await
                    .unwrap()
                    .unwrap();
                if run.id != failed.id && run.status == "completed" {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("wake retries after the backoff and completes");
        assert_eq!(
            llm.requests()
                .iter()
                .filter(|request| request["stream"] == true)
                .count(),
            3
        );
        let messages = db::messages::list_messages(&app.pool, &conv).await.unwrap();
        assert_eq!(
            messages
                .iter()
                .filter(|message| message.id == message_id)
                .count(),
            1
        );
        assert!(
            db::background::pending_wake(&app.pool, &conv)
                .await
                .unwrap()
                .is_none()
        );
        app.pool.close().await;
    }
}

#[tokio::test]
async fn cancelled_wake_does_not_restart_automatically() {
    use oct_llm_provider::core::{Message, Role};
    let env = TestEnv::new().await;
    let conv = new_conversation(&env.pool, env.dir.path()).await;
    let msg = db::background::insert_notification_marking_tasks(
        &env.pool,
        &conv,
        &Message::text(Role::User, "notification"),
        None,
        &[],
    )
    .await
    .unwrap();
    assert_eq!(
        db::background::pending_wake(&env.pool, &conv)
            .await
            .unwrap(),
        Some(msg.clone())
    );
    let run = db::runs::create_run(
        &env.pool,
        db::runs::NewRun {
            conversation_id: conv.clone(),
            kind: "root",
            parent_run_id: None,
            root_run_id: None,
            parent_tool_call_id: None,
            provider_id: None,
            model_id: None,
            context_window: None,
            description: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        db::background::pending_wake(&env.pool, &conv)
            .await
            .unwrap(),
        Some(msg)
    );
    db::runs::finish_run(
        &env.pool,
        &run.id,
        db::runs::RunFinalStatus::Cancelled,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(
        db::background::pending_wake(&env.pool, &conv)
            .await
            .unwrap()
            .is_none()
    );
}
