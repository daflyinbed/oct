//! Opt-in manual probe. Cargo tests never connect to configured servers.
use anyhow::{Context, Result, ensure};
use clap::Parser;
use oct_agent::agent::EventHub;
use oct_agent::codemode::{Prepared, catalog::Catalog};
use oct_agent::mcp::tool::McpTool;
use oct_agent::permissions::gate::PermissionGate;
use oct_agent::tools::{AgentTool, ToolContext};
use oct_mcp::{HttpServerConfig, McpConnection, StdioServerConfig};
use serde_json::{Value, json};
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

#[derive(Parser)]
struct Args {
    /// Existing OpenCode/ZCode JSON/JSONC file; it is never modified.
    #[arg(long)]
    config: PathBuf,
    /// Explicit server names; no unselected server is contacted.
    #[arg(long, required = true)]
    server: Vec<String>,
    #[arg(long)]
    include_disabled: bool,
    /// New directory for the generated SDK, schema fixture and diagnostics.
    #[arg(long)]
    output: PathBuf,
    /// Optional real tool call through execute_code (select exactly one server).
    #[arg(long)]
    call: Option<String>,
    #[arg(long, default_value = "{}")]
    arguments: String,
}

async fn connect(value: &Value) -> Result<Arc<dyn McpConnection>> {
    let timeout = Duration::from_secs(45);
    let connection = match value["type"].as_str() {
        Some("local") => {
            let command: Vec<String> = serde_json::from_value(value["command"].clone())?;
            ensure!(!command.is_empty(), "Empty MCP command");
            let env = serde_json::from_value::<HashMap<String, String>>(
                value.get("environment").cloned().unwrap_or(json!({})),
            )?;
            oct_mcp::connect_stdio(
                &StdioServerConfig {
                    command: command[0].clone(),
                    args: command[1..].to_vec(),
                    env,
                },
                timeout,
            )
            .await?
        }
        Some("remote" | "http") => {
            oct_mcp::connect_http(
                &HttpServerConfig {
                    url: value["url"].as_str().context("MCP URL missing")?.into(),
                    headers: serde_json::from_value(
                        value.get("headers").cloned().unwrap_or(json!({})),
                    )?,
                },
                timeout,
            )
            .await?
        }
        _ => anyhow::bail!("Unsupported OpenCode MCP type"),
    };
    Ok(Arc::from(connection))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(
        args.call.is_none() || args.server.len() == 1,
        "--call requires exactly one --server"
    );
    ensure!(!args.output.exists(), "Choose a new --output directory");
    let config: Value = jsonc_parser::parse_to_serde_value(
        &std::fs::read_to_string(&args.config)?,
        &Default::default(),
    )?;
    let mut connections = Vec::new();
    let mut tools: Vec<Arc<dyn AgentTool>> = Vec::new();
    let mut fixture = Vec::new();
    for name in &args.server {
        let servers = config["mcp"].get("servers").unwrap_or(&config["mcp"]);
        let value = servers
            .get(name)
            .context("Selected server not in configuration")?;
        ensure!(
            value["enabled"] != false || args.include_disabled,
            "{name} is disabled; use --include-disabled to explicitly probe it"
        );
        let connection = connect(value)
            .await
            .with_context(|| format!("Connecting {name}"))?;
        let result: Result<()> = async {
            let entries = tokio::time::timeout(Duration::from_secs(45), connection.list_tools()).await??;
            println!("{name}: {} tools", entries.len());
            let mut schemas = Vec::new();
            for info in entries {
                schemas.push(json!({"name":info.name,"description":info.description,"inputSchema":info.input_schema,"outputSchema":info.output_schema}));
                tools.push(Arc::new(McpTool::new(
                    oct_mcp::qualified_tool_name(name, &info.name), info, name.clone(),
                    connection.clone(), Duration::from_secs(30),
                ).with_server_id(format!("manual-probe:{name}"))));
            }
            fixture.push(json!({"server":name,"tools":schemas}));
            Ok(())
        }.await;
        if let Err(error) = result {
            connection.shutdown().await;
            return Err(error);
        }
        connections.push(connection);
    }
    let result: Result<()> = async {
        let start = std::time::Instant::now();
        let catalog = Catalog::build(&tools)?;
        let generation_ms = start.elapsed().as_secs_f64() * 1000.0;
        let warm = std::time::Instant::now();
        let _cached = Catalog::build(&tools)?;
        let cache_ms = warm.elapsed().as_secs_f64() * 1000.0;
        let prepared = Prepared::new(args.output.clone(), "probe", "run", &tools).await?;
        let sdk = prepared.root.join("sdk");
        let bytes: usize = catalog.files.values().map(String::len).sum();
        let max_file = catalog.files.values().map(String::len).max().unwrap_or(0);
        println!("sdk: {}\nfiles: {}, bytes: {}, largest_file: {}, generation_ms: {generation_ms:.2}, cached_ms: {cache_ms:.2}", sdk.display(),catalog.files.len(),bytes,max_file);
        let fixture_path = args.output.join("catalog-fixture.json");
        std::fs::write(&fixture_path, serde_json::to_vec_pretty(&fixture)?)?;
        #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; std::fs::set_permissions(&fixture_path,std::fs::Permissions::from_mode(0o600))?; }
        if let Some(name) = &args.call {
            let index: Value = serde_json::from_str(&catalog.files["index.json"])?;
            let server = &index[0];
            let tool_index: Value = serde_json::from_str(&catalog.files[server["toolIndex"].as_str().context("Missing toolIndex")?])?;
            let tool = tool_index.as_array().unwrap().iter().find(|t|t["name"] == *name).context("Unknown requested tool")?;
            let input: Value = serde_json::from_str(&args.arguments)?;
            // Return counts only, keeping browser/IDA data out of the terminal.
            let code = format!("import * as tools from {}; const r = await tools[{}]({}); text({{textBytes:r.text.length, structuredKeys:Object.keys(r.structuredContent ?? {{}}), images:r.images.length, isError:r.isError}});",server["importSpecifier"],tool["export"],input);
            let tool = prepared.tool(Arc::new(tools.clone()), PermissionGate::permissive());
            let ctx = ToolContext::new("manual-probe", Arc::new(EventHub::new()), tokio_util::sync::CancellationToken::new());
            let output = tool.execute(json!({"code":code}), &ctx).await?;
            println!("{}",output.content);
            ensure!(!output.is_error,"Code-mode smoke call failed");
        }
        Ok(())
    }.await;
    for connection in connections {
        connection.shutdown().await;
    }
    result
}
