//! Main/worker code mode over each run's effective builtin and MCP identities.
pub mod catalog;
pub(crate) mod diagnostics;
mod storage;
use crate::agent::{AgentEvent, EventHub};
use crate::permissions::gate::PermissionGate;
use crate::tools::{AgentTool, OutputStream, ToolContext, ToolImage, ToolOutput};
use anyhow::{Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, atomic::AtomicUsize};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, utoipa::ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpToolMode {
    // `#[default]` MUST stay Direct: v1 WorkerSnapshot rows lack `tool_mode`
    // and decode through serde's default; flipping this to Code would reject
    // every pre-code-mode snapshot in the compatibility matrix. The runtime
    // default lives in `mode()` below, not here.
    #[default]
    Direct,
    Code,
}

pub const SETTING: &str = "mcp_tool_mode";

impl McpToolMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Code => "code",
        }
    }
}

pub async fn project_mode(pool: &sqlx::SqlitePool, project: &str) -> Result<Option<McpToolMode>> {
    let key = SETTING;
    let value = sqlx::query_scalar!(
        "SELECT value FROM project_settings WHERE project_id = ? AND key = ?",
        project,
        key
    )
    .fetch_optional(pool)
    .await?;
    Ok(match value.as_deref() {
        Some("code") => Some(McpToolMode::Code),
        Some("direct") => Some(McpToolMode::Direct),
        _ => None,
    })
}

pub async fn set_project_mode(
    pool: &sqlx::SqlitePool,
    project: &str,
    mode: Option<McpToolMode>,
) -> Result<()> {
    let key = SETTING;
    let value = mode.map(McpToolMode::as_str).unwrap_or("inherit");
    sqlx::query!("INSERT INTO project_settings (project_id, key, value) VALUES (?, ?, ?) ON CONFLICT(project_id, key) DO UPDATE SET value = excluded.value, updated_at = CURRENT_TIMESTAMP", project, key, value).execute(pool).await?;
    Ok(())
}

pub async fn mode(pool: &sqlx::SqlitePool, project: Option<&str>) -> Result<McpToolMode> {
    if let Some(project) = project
        && let Some(mode) = project_mode(pool, project).await?
    {
        return Ok(mode);
    }
    Ok(
        if crate::db::settings::get(pool, SETTING).await?.as_deref() == Some("direct") {
            McpToolMode::Direct
        } else {
            McpToolMode::Code
        },
    )
}

pub struct Prepared {
    pub catalog: Arc<catalog::Catalog>,
    pub root: PathBuf,
    pub sdk: PathBuf,
    pub worker: PathBuf,
    _storage: storage::Lease,
}

impl Prepared {
    pub async fn new(
        base: PathBuf,
        conversation: &str,
        run: &str,
        tools: &[Arc<dyn AgentTool>],
    ) -> Result<Self> {
        Self::prepare(base, conversation, run, "sdk", tools).await
    }

    pub(crate) async fn for_worker(
        base: PathBuf,
        conversation: &str,
        run: &str,
        epoch: i64,
        tools: &[Arc<dyn AgentTool>],
    ) -> Result<Self> {
        // A recovered worker keeps its run id and cell ledger, but gets a
        // fresh SDK directory instead of overwriting the crashed epoch's SDK.
        Self::prepare(base, conversation, run, &format!("sdk-{epoch}"), tools).await
    }

    async fn prepare(
        base: PathBuf,
        conversation: &str,
        run: &str,
        sdk_name: &str,
        tools: &[Arc<dyn AgentTool>],
    ) -> Result<Self> {
        let worker = oct_codemode::worker_path().ok_or_else(|| {
            anyhow::anyhow!(
                "Code mode helper is unavailable; build oct-codemode with --features runtime"
            )
        })?;
        let catalog = Arc::new(catalog::Catalog::build(tools)?);
        let root = base.join(conversation).join(run);
        let sdk = root.join(sdk_name);
        let storage = storage::acquire(&base, &root).await?;
        tokio::fs::create_dir_all(root.join("cells")).await?;
        catalog.publish(&sdk).await?;
        Ok(Self {
            catalog,
            root,
            sdk,
            worker,
            _storage: storage,
        })
    }

    pub fn expose(
        self,
        base: Arc<Vec<Arc<dyn AgentTool>>>,
        gate: Arc<PermissionGate>,
    ) -> Arc<Vec<Arc<dyn AgentTool>>> {
        // Keep native discovery/editing and task controls available directly:
        // the read tools double as the SDK browser (the run's read-only root
        // covers the published SDK). MCP schemas are replaced by the single
        // execute_code surface.
        let mut visible: Vec<_> = base
            .iter()
            .filter(|tool| {
                !matches!(
                    tool.selector(),
                    crate::agent_settings::ToolSelector::Mcp { .. }
                )
            })
            .cloned()
            .collect();
        visible.push(self.tool(base, gate));
        Arc::new(visible)
    }

    /// The system-prompt section naming the loaded server groups and the SDK
    /// directory the run's read tools should browse.
    pub fn prompt_section(&self) -> crate::agent::prompt::CodeModeSection {
        crate::agent::prompt::CodeModeSection {
            sdk_path: self.sdk.display().to_string(),
            servers: self
                .catalog
                .servers
                .iter()
                .map(|group| (group.server.clone(), group.tools))
                .collect(),
        }
    }

    pub fn tool(
        self,
        base: Arc<Vec<Arc<dyn AgentTool>>>,
        gate: Arc<PermissionGate>,
    ) -> Arc<dyn AgentTool> {
        let description = format!(
            "Execute a TypeScript ES module to orchestrate the authorized builtin file/bash and MCP tools. Discover this run's interfaces by read_file on {}/index.json (module import specifiers), then grep/read the .d.ts declarations and README.md under that directory. Import the exact virtual .js modules listed in index.json; relative imports start at the SDK root. Await every call; use text(value), console.log(value), image({{mimeType,data}}) for explicit output. Intermediate results stay in code. File/bash operations go through host tools with normal approvals, shared file locks and this agent's read state; JS has no direct filesystem/network/subprocess/npm. Task delegation is only available through the outer task tool; workers cannot start background commands. Cancellation does not undo side effects. Fresh VM per call; top-level await supported, top-level return unsupported.",
            self.sdk.display()
        );
        Arc::new(CodeTool {
            prepared: self,
            gate,
            _base: base,
            description,
        })
    }
}

struct CodeTool {
    prepared: Prepared,
    gate: Arc<PermissionGate>,
    // TaskTool's weak parent slot must continue to resolve the unfiltered set.
    _base: Arc<Vec<Arc<dyn AgentTool>>>,
    description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
pub struct NestedCall {
    pub id: String,
    pub name: String,
    pub title: String,
    pub status: String,
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

struct Host {
    tools: BTreeMap<String, Arc<dyn AgentTool>>,
    gate: Arc<PermissionGate>,
    hub: Arc<EventHub>,
    context: ToolContext,
    approvals: Arc<AtomicUsize>,
    calls: Arc<Mutex<Vec<NestedCall>>>,
    images: Mutex<Vec<ToolImage>>,
    trace: Mutex<std::fs::File>,
    output: Mutex<std::fs::File>,
    _cell_storage: storage::CellLease,
}

impl Host {
    fn record(&self, call: &NestedCall) -> Result<()> {
        use std::io::Write;
        let mut file = self.trace.lock().unwrap_or_else(|p| p.into_inner());
        let row = serde_json::to_vec(call)?;
        ensure!(
            file.metadata()?.len().saturating_add(row.len() as u64 + 1) <= 8 * 1024 * 1024,
            "Code mode call ledger exceeds artifact budget"
        );
        file.write_all(&row)?;
        file.write_all(b"\n")?;
        file.flush()?;
        self.hub.publish(AgentEvent::NestedToolUpdate {
            parent_call_id: self.context.call_id().into(),
            call: call.clone(),
        });
        Ok(())
    }
    fn settle_unknown(&self) -> Vec<NestedCall> {
        let mut rows = self.calls.lock().unwrap_or_else(|p| p.into_inner());
        for row in rows.iter_mut().filter(|r| r.status == "running") {
            row.status = "outcome_unknown".into();
            row.duration_ms = row
                .started_at_ms
                .map(|start| (chrono::Utc::now().timestamp_millis() as u64).saturating_sub(start));
            let _ = self.record(row);
        }
        rows.clone()
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.settle_unknown();
    }
}

#[async_trait]
impl oct_codemode::Host for Host {
    async fn call(&self, seq: u64, key: String, args: Value) -> Result<Value> {
        let tool = self.tools.get(&key).ok_or_else(|| {
            oct_codemode::CallError::new(
                "tool_unavailable",
                "Tool is not available in this code-mode catalog",
            )
        })?;
        let ctx = self
            .context
            .nested(format!("{}/{seq}", self.context.call_id()));
        let started = std::time::Instant::now();
        let mut call = NestedCall {
            id: ctx.call_id().into(),
            name: tool.name().into(),
            title: tool.title(&args).chars().take(2048).collect(),
            status: "running".into(),
            error: None,
            error_code: None,
            started_at_ms: Some(chrono::Utc::now().timestamp_millis() as u64),
            duration_ms: None,
        };
        self.record(&call)?;
        self.calls.lock().unwrap().push(call.clone());
        let output = crate::tools::dispatch::nested(
            tool,
            args,
            &ctx,
            &self.gate,
            &self.hub,
            &self.approvals,
        )
        .await;
        ctx.flush();
        call.duration_ms = Some(started.elapsed().as_millis() as u64);
        call.error_code = output
            .as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<oct_codemode::CallError>())
            .map(|e| e.code.clone());
        call.status = if output.is_ok() {
            "completed"
        } else if matches!(
            call.error_code.as_deref(),
            Some("tool_timeout" | "transport_error" | "cancelled")
        ) {
            "outcome_unknown"
        } else {
            "failed"
        }
        .into();
        call.error = output
            .as_ref()
            .err()
            .map(|e| e.to_string().chars().take(2048).collect());
        if let Some(row) = self
            .calls
            .lock()
            .unwrap()
            .iter_mut()
            .find(|row| row.id == call.id)
        {
            *row = call.clone();
        }
        self.record(&call)?;
        Ok(crate::tools::dispatch::result_value(output?))
    }
    fn emit(&self, text: &str) -> Result<()> {
        use std::io::Write;
        let mut output = self.output.lock().unwrap_or_else(|p| p.into_inner());
        output.write_all(text.as_bytes())?;
        output.flush()?;
        self.context.emit_output_delta(OutputStream::Stdout, text);
        Ok(())
    }
    fn image(&self, mime_type: String, data: String) -> Result<()> {
        ensure!(
            matches!(
                mime_type.as_str(),
                "image/png" | "image/jpeg" | "image/gif" | "image/webp"
            ),
            "Unsupported output image type"
        );
        ensure!(data.len() <= 5 * 1024 * 1024, "Image exceeds output limit");
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(&data)
            .map_err(|_| anyhow::anyhow!("Output image must contain valid base64"))?;
        let mut images = self.images.lock().unwrap();
        ensure!(images.len() < 8, "Image count exceeds output limit");
        images.push(ToolImage { mime_type, data });
        Ok(())
    }
}

#[async_trait]
impl AgentTool for CodeTool {
    fn name(&self) -> &str {
        "execute_code"
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn title(&self, _: &Value) -> String {
        "Code mode".into()
    }
    fn interrupted_output(&self, call_id: &str) -> Option<ToolOutput> {
        let dir = self
            .prepared
            .root
            .join("cells")
            .join(catalog::digest(call_id));
        diagnostics::interrupted_cell(&dir, call_id, "cancelled")
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{"code":{"type":"string"},"timeout_ms":{"type":"integer","minimum":1,"maximum":300000}},"required":["code"],"additionalProperties":false})
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let started = std::time::Instant::now();
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            code: String,
            timeout_ms: Option<u64>,
        }
        let args: Args = serde_json::from_value(args)?;
        ensure!(
            args.code.len() <= oct_codemode::MAX_CODE,
            "Code input exceeds limit"
        );
        let timeout_ms = args.timeout_ms.unwrap_or(120_000);
        ensure!(
            (1..=300_000).contains(&timeout_ms),
            "timeout_ms must be between 1 and 300000"
        );
        static CELLS: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
        let _permit = tokio::select! {
            permit=CELLS.get_or_init(||tokio::sync::Semaphore::new(4)).acquire()=>permit?,
            _=ctx.cancel_token().cancelled()=>anyhow::bail!("Code execution cancelled while queued"),
        };
        let dir = self
            .prepared
            .root
            .join("cells")
            .join(catalog::digest(ctx.call_id()));
        let cell_storage = storage::reserve_cell(&self.prepared.root).await?;
        tokio::fs::create_dir_all(&dir).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).await?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        {
            use std::io::Write;
            options
                .open(dir.join("main.ts"))?
                .write_all(args.code.as_bytes())?;
        }
        let trace = options.open(dir.join("calls.jsonl"))?;
        let output = options.open(dir.join("output.log"))?;
        let calls = Arc::new(Mutex::new(Vec::new()));
        let host = Arc::new(Host {
            tools: self.prepared.catalog.tools.clone(),
            gate: self.gate.clone(),
            hub: ctx.hub(),
            context: ctx.clone(),
            approvals: Arc::new(AtomicUsize::new(0)),
            calls: calls.clone(),
            images: Mutex::new(Vec::new()),
            trace: Mutex::new(trace),
            output: Mutex::new(output),
            _cell_storage: cell_storage,
        });
        let result = oct_codemode::run(
            &self.prepared.worker,
            oct_codemode::Init {
                code: args.code,
                modules: self.prepared.catalog.modules.as_ref().clone(),
                timeout_ms,
            },
            host.clone(),
            ctx.cancel_token().clone(),
            host.approvals.clone(),
        )
        .await
        .unwrap_or_else(|error| oct_codemode::RunResult {
            error: Some(format!("{error:#}")),
            ..Default::default()
        });
        let rows = host.settle_unknown();
        let mut content = format!(
            "Script {} ({} tool calls).\n",
            if result.error.is_some() {
                "failed"
            } else {
                "completed"
            },
            result.calls
        );
        if let Some(error) = &result.error {
            content.push_str(error);
            content.push('\n');
        }
        content.push_str(&crate::tools::truncate::truncate_middle(
            &result.output,
            32 * 1024,
        ));
        let failed = rows.iter().filter(|r| r.status != "completed").count();
        if failed > 0 {
            content.push_str(&format!("\n{failed} nested calls failed or have unknown outcomes; inspect calls.jsonl before retrying."));
        }
        if result.output.len() > 32 * 1024 || failed > 0 || result.error.is_some() {
            content.push_str(&format!(
                "\noutput_file: {}\ntrace_file: {}\nsource_file: {}",
                dir.join("output.log").display(),
                dir.join("calls.jsonl").display(),
                dir.join("main.ts").display()
            ));
        }
        let details = json!({"kind":"code_mode","calls":rows,"output_file":dir.join("output.log"),"trace_file":dir.join("calls.jsonl"),"source_file":dir.join("main.ts"),"wall_time_ms":started.elapsed().as_millis() as u64,"bytes":result.output.len(),"truncated":result.output.len()>32*1024,"original_bytes":result.output.len()});
        let images = host.images.lock().unwrap().clone();
        Ok(if result.error.is_some() {
            ToolOutput::error_with_details(content, details)
        } else {
            ToolOutput::success_with_details(content, details)
        }
        .with_images(images))
    }
}
