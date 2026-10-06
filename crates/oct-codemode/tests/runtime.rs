#![cfg(feature = "runtime")]
use anyhow::Result;
use async_trait::async_trait;
use oct_codemode::{Host, Init};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio_util::sync::CancellationToken;

struct MockHost {
    calls: AtomicUsize,
    barrier: Option<tokio::sync::Barrier>,
}
#[async_trait]
impl Host for MockHost {
    async fn call(&self, _: u64, _: String, args: Value) -> Result<Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(barrier) = &self.barrier {
            barrier.wait().await;
        }
        Ok(json!({"structuredContent": args, "text": "mock", "isError": false, "images": []}))
    }
    fn emit(&self, _: &str) -> Result<()> {
        Ok(())
    }
    fn image(&self, _: String, _: String) -> Result<()> {
        Ok(())
    }
}

async fn eval(
    code: &str,
    parallel: bool,
    timeout_ms: u64,
) -> (oct_codemode::RunResult, Arc<MockHost>) {
    let host = Arc::new(MockHost {
        calls: AtomicUsize::new(0),
        barrier: parallel.then(|| tokio::sync::Barrier::new(2)),
    });
    let result = eval_with_host(
        code,
        host.clone(),
        timeout_ms,
        Arc::new(AtomicUsize::new(0)),
    )
    .await;
    (result, host)
}

async fn eval_with_host(
    code: &str,
    host: Arc<dyn Host>,
    timeout_ms: u64,
    approvals: Arc<AtomicUsize>,
) -> oct_codemode::RunResult {
    let init = Init {
        code: code.into(),
        modules: BTreeMap::from([(
            "mcp/test.js".into(),
            "export const echo = args => globalThis.__oct_call('echo', args);".into(),
        )]),
        timeout_ms,
    };
    oct_codemode::run(
        std::path::Path::new(env!("CARGO_BIN_EXE_oct-codemode-worker")),
        init,
        host,
        CancellationToken::new(),
        approvals,
    )
    .await
    .unwrap()
}

async fn spawn_stdio_worker() -> (
    tokio::process::Child,
    tokio::process::ChildStdin,
    tokio::process::ChildStdout,
) {
    use oct_codemode::{FromWorker, PROTOCOL_VERSION, read_frame};
    use std::process::Stdio;
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_oct-codemode-worker"))
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let hello: FromWorker =
        tokio::time::timeout(std::time::Duration::from_secs(5), read_frame(&mut stdout))
            .await
            .unwrap()
            .unwrap();
    assert!(matches!(
        hello,
        FromWorker::Hello {
            protocol_version: PROTOCOL_VERSION
        }
    ));
    (child, stdin, stdout)
}

#[tokio::test]
async fn stdio_protocol_keeps_console_output_framed_across_pipe_buffers() {
    use oct_codemode::{FromWorker, ToWorker, read_frame, write_frame};
    let (mut child, mut stdin, mut stdout) = spawn_stdio_worker().await;
    write_frame(
        &mut stdin,
        &ToWorker::Init(Init {
            code: r#"console.log('console'); const value: string = '界\n'.repeat(65536); text(value);"#
                .into(),
            modules: BTreeMap::new(),
            timeout_ms: 5000,
        }),
    )
    .await
    .unwrap();
    let mut output = String::new();
    loop {
        let message: FromWorker =
            tokio::time::timeout(std::time::Duration::from_secs(5), read_frame(&mut stdout))
                .await
                .unwrap()
                .unwrap();
        match message {
            FromWorker::Emit { text } => output.push_str(&text),
            FromWorker::Done { error } => {
                assert_eq!(error, None);
                break;
            }
            other => panic!("Unexpected stdout frame: {other:?}"),
        }
    }
    assert_eq!(output, format!("console\n{}\n", "界\n".repeat(65536)));
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
}

#[tokio::test]
async fn stdin_eof_stops_worker_before_init_and_during_javascript() {
    use oct_codemode::{FromWorker, ToWorker, read_frame, write_frame};
    for code in [None, Some("text('ready'); while (true) {};")] {
        let (mut child, mut stdin, mut stdout) = spawn_stdio_worker().await;
        if let Some(code) = code {
            write_frame(
                &mut stdin,
                &ToWorker::Init(Init {
                    code: code.into(),
                    modules: BTreeMap::new(),
                    timeout_ms: 5000,
                }),
            )
            .await
            .unwrap();
            let message: FromWorker =
                tokio::time::timeout(std::time::Duration::from_secs(5), read_frame(&mut stdout))
                    .await
                    .unwrap()
                    .unwrap();
            assert!(matches!(message, FromWorker::Emit { text } if text == "ready\n"));
        }
        // Closing the parent's write end must terminate the helper without
        // relying on an OS-specific parent-death signal or a JS yield point.
        drop(stdin);
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(!status.success());
    }
}

#[tokio::test]
async fn typescript_modules_parallel_calls_and_only_explicit_output() {
    let (result, host) = eval("import {echo} from './mcp/test.js'; const n: number = 3; const results = await Promise.all([echo({n}), echo({n:4})]); text(results.map(r => r.structuredContent.n));", true, 5000).await;
    assert_eq!(result.error, None);
    assert_eq!(result.output, "[3,4]\n");
    assert_eq!(host.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn module_and_global_capabilities_are_closed() {
    for spec in [
        "file:///etc/passwd",
        "https://example.com/x.js",
        "node:fs",
        "ext:core/ops",
        "ext:core/mod.js",
        "../secret.js",
        "./mcp/test.d.ts",
    ] {
        let (result, host) = eval(&format!("await import({spec:?});"), false, 2000).await;
        assert!(result.error.is_some(), "{spec}: {result:?}");
        assert_eq!(host.calls.load(Ordering::SeqCst), 0);
    }
    let (result, _) = eval(
        "text([typeof Deno, typeof fetch, typeof process, typeof require, typeof WebAssembly]);",
        false,
        2000,
    )
    .await;
    assert_eq!(result.error, None);
    assert_eq!(
        result.output,
        "[\"undefined\",\"undefined\",\"undefined\",\"undefined\",\"undefined\"]\n"
    );
}

#[tokio::test]
async fn spinning_or_unsettleable_script_does_not_poison_next_cell() {
    for code in [
        "while(true) {}",
        "await new Promise(() => {});",
        "await Promise.resolve(); while(true) await Promise.resolve();",
    ] {
        let (result, _) = eval(code, false, 150).await;
        assert!(result.error.is_some(), "{result:?}");
    }
    let (result, _) = eval("text('healthy');", false, 2000).await;
    assert_eq!(result.error, None);
    assert_eq!(result.output, "healthy\n");
}

#[tokio::test]
async fn script_error_keeps_output_and_original_source_location() {
    let (result, _) = eval(
        "const n: number = 1;\ntext(n);\nthrow new Error('example failure');",
        false,
        2000,
    )
    .await;
    assert_eq!(result.output, "1\n");
    let error = result.error.unwrap();
    assert!(error.contains("example failure"), "{error}");
    assert!(error.contains("main.ts:3"), "{error}");
}

#[tokio::test]
async fn input_budget_failure_can_be_caught_and_next_call_is_valid() {
    let (result,host)=eval("import {echo} from './mcp/test.js'; try { await echo({data:'x'.repeat(1024*1024)}); } catch (e) {text(e.name);} const r=await echo({n:7}); text(r.structuredContent.n);",false,5000).await;
    assert_eq!(result.error, None, "{result:?}");
    assert_eq!(result.output, "ToolCallError\n7\n");
    assert_eq!(host.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn selectors_and_pending_payloads_are_bounded_while_calls_wait() {
    struct WaitingHost {
        calls: AtomicUsize,
        waiting: Arc<AtomicUsize>,
        release: tokio::sync::Semaphore,
        emitted: tokio::sync::mpsc::UnboundedSender<String>,
    }
    #[async_trait]
    impl Host for WaitingHost {
        async fn call(&self, _: u64, _: String, args: Value) -> Result<Value> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.waiting.fetch_add(1, Ordering::SeqCst);
            self.release.acquire().await?.forget();
            self.waiting.fetch_sub(1, Ordering::SeqCst);
            Ok(json!({"structuredContent": args}))
        }
        fn emit(&self, text: &str) -> Result<()> {
            self.emitted.send(text.to_owned())?;
            Ok(())
        }
        fn image(&self, _: String, _: String) -> Result<()> {
            Ok(())
        }
    }
    let waiting = Arc::new(AtomicUsize::new(0));
    let (emitted, mut output) = tokio::sync::mpsc::unbounded_channel();
    let host = Arc::new(WaitingHost {
        calls: AtomicUsize::new(0),
        waiting: waiting.clone(),
        release: tokio::sync::Semaphore::new(0),
        emitted,
    });
    let code = format!(
        r#"
        import {{echo}} from './mcp/test.js';
        const calls = Array.from({{length:{}}}, () => echo({{data:'x'.repeat({})}}));
        try {{ await globalThis.__oct_call('x'.repeat({}), {{}}); }}
        catch (e) {{ text(e.name + ': ' + e.message); }}
        try {{ await echo({{data:'x'.repeat({})}}); }}
        catch (e) {{ text(e.name + ': ' + e.message); }}
        text('release');
        await Promise.all(calls);
        text((await echo({{n:7}})).structuredContent.n);
    "#,
        oct_codemode::CONCURRENCY,
        oct_codemode::MAX_ARGS - 64,
        oct_codemode::MAX_SELECTOR + 1,
        oct_codemode::MAX_ARGS - 64
    );
    let run_host = host.clone();
    let call = tokio::spawn(async move { eval_with_host(&code, run_host, 5000, waiting).await });
    let mut rejected = Vec::new();
    loop {
        let text = tokio::time::timeout(std::time::Duration::from_secs(10), output.recv())
            .await
            .unwrap()
            .unwrap();
        if text == "release\n" {
            break;
        }
        rejected.push(text);
    }
    assert_eq!(rejected.len(), 2);
    assert!(rejected[0].contains("ToolCallError: Code mode selector exceeds limit"));
    assert!(rejected[1].contains("ToolCallError: Code mode pending call payload budget exceeded"));
    host.release.add_permits(oct_codemode::CONCURRENCY + 1);
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), call)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.error, None, "{result:?}");
    assert!(result.output.ends_with("release\n7\n"), "{result:?}");
    assert_eq!(
        host.calls.load(Ordering::SeqCst),
        oct_codemode::CONCURRENCY + 1
    );
    assert_eq!(result.calls, (oct_codemode::CONCURRENCY + 2) as u64);
}

#[tokio::test]
async fn oversized_success_and_error_replies_are_catchable() {
    struct ReplyHost;
    #[async_trait]
    impl Host for ReplyHost {
        async fn call(&self, _: u64, _: String, args: Value) -> Result<Value> {
            match args["kind"].as_str().unwrap() {
                "success" => Ok(json!("x".repeat(oct_codemode::MAX_RESULT))),
                "error" => anyhow::bail!("{}", "x".repeat(oct_codemode::MAX_RESULT + 1)),
                "frame" => anyhow::bail!("{}", "x".repeat(33 * 1024 * 1024)),
                // Escaping puts this over the JSON budget despite its small raw size.
                "escaped" => anyhow::bail!("{}", "\0".repeat(oct_codemode::MAX_RESULT / 6 + 1)),
                "small" => anyhow::bail!("expected failure"),
                _ => Ok(json!({"structuredContent":{"n":7}})),
            }
        }
        fn emit(&self, _: &str) -> Result<()> {
            Ok(())
        }
        fn image(&self, _: String, _: String) -> Result<()> {
            Ok(())
        }
    }
    let result = eval_with_host(
        r#"
        import {echo} from './mcp/test.js';
        for (const kind of ['success', 'error', 'frame', 'escaped', 'small']) {
            try { await echo({kind}); }
            catch (e) { text(e.name + ': ' + e.message); }
        }
        text((await echo({kind:'healthy'})).structuredContent.n);
    "#,
        Arc::new(ReplyHost),
        10000,
        Arc::new(AtomicUsize::new(0)),
    )
    .await;
    assert_eq!(result.error, None, "{result:?}");
    assert_eq!(
        result.output,
        format!(
            "{}ToolCallError: expected failure\n7\n",
            "ToolCallError: Tool result exceeds code mode budget; request a smaller page\n"
                .repeat(4)
        )
    );
    assert_eq!(result.calls, 6);
}

#[tokio::test]
async fn output_limit_keeps_prior_output_and_heap_failure_is_contained() {
    let (result, _) = eval(
        "text('before'); text('x'.repeat(16*1024*1024));",
        false,
        5000,
    )
    .await;
    assert_eq!(result.output, "before\n");
    assert!(result.error.unwrap().contains("output exceeds limit"));
    let (result, _) = eval(
        "const xs=[]; while(true) xs.push(new Array(200_000).fill('x'));",
        false,
        5000,
    )
    .await;
    assert!(result.error.is_some());
    let (result, _) = eval("text('healthy');", false, 2000).await;
    assert_eq!(result.error, None);
}

#[tokio::test]
async fn unawaited_call_is_an_error_and_calls_are_bounded() {
    let (result, _) = eval(
        "import {echo} from './mcp/test.js'; echo({n:1});",
        // One call cannot pass the two-party barrier. Keep the reply pending
        // so this assertion does not race a fast successful IPC reply.
        true,
        5000,
    )
    .await;
    assert!(result.error.is_some(), "{result:?}");
    let (result, host) = eval(
        "import {echo} from './mcp/test.js'; for(let n=0;n<129;n++) await echo({n});",
        false,
        5000,
    )
    .await;
    assert!(result.error.unwrap().contains("call budget exceeded"));
    assert_eq!(host.calls.load(Ordering::SeqCst), 128);
}

#[tokio::test]
async fn approval_wait_pauses_wall_deadline() {
    struct ApprovalHost {
        waiting: Arc<AtomicUsize>,
        started: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    #[async_trait]
    impl Host for ApprovalHost {
        async fn call(&self, _: u64, _: String, _: Value) -> Result<Value> {
            self.waiting.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            self.release.notified().await;
            self.waiting.fetch_sub(1, Ordering::SeqCst);
            Ok(json!(42))
        }
        fn emit(&self, _: &str) -> Result<()> {
            Ok(())
        }
        fn image(&self, _: String, _: String) -> Result<()> {
            Ok(())
        }
    }
    let waiting = Arc::new(AtomicUsize::new(0));
    let host = Arc::new(ApprovalHost {
        waiting: waiting.clone(),
        started: Default::default(),
        release: Default::default(),
    });
    let init = Init {
        code: "text(await globalThis.__oct_call('approve', {}));".into(),
        modules: BTreeMap::new(),
        timeout_ms: 1000,
    };
    let call = tokio::spawn(oct_codemode::run(
        std::path::Path::new(env!("CARGO_BIN_EXE_oct-codemode-worker")),
        init,
        host.clone(),
        CancellationToken::new(),
        waiting,
    ));
    host.started.notified().await;
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(10)).await;
    tokio::task::yield_now().await;
    assert!(!call.is_finished());
    tokio::time::resume();
    host.release.notify_one();
    let result = call.await.unwrap().unwrap();
    assert_eq!(result.error, None, "{result:?}");
    assert_eq!(result.output, "42\n");
}
