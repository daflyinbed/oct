//! Declaration files and virtual bindings are two views of one run catalog.
use crate::agent_settings::ToolSelector;
use crate::tools::AgentTool;
use anyhow::{Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

pub const VERSION: u32 = crate::tool_contract::CODE_CATALOG_VERSION;
const GENERATOR: &str = "oct-sdk-v3";
const SPLIT_BYTES: usize = 32 * 1024;
const SPLIT_TOOLS: usize = 24;
const CATALOG_BYTES: usize = 32 * 1024 * 1024;
const CACHE_BYTES: usize = 64 * 1024 * 1024;

pub struct Catalog {
    pub fingerprint: String,
    pub files: Arc<BTreeMap<String, String>>,
    pub modules: Arc<BTreeMap<String, String>>,
    /// Server groups in prompt order (builtin first), for prompt rendering.
    pub servers: Vec<ServerSummary>,
    // Never cached: each run binds its own effective tool objects and permissions.
    pub tools: BTreeMap<String, Arc<dyn AgentTool>>,
}

/// One sealed server group of a run's SDK.
#[derive(Debug, Clone)]
pub struct ServerSummary {
    pub kind: &'static str,
    pub server: String,
    pub tools: usize,
}

#[derive(Serialize)]
struct Descriptor {
    kind: &'static str,
    server_id: String,
    server: String,
    original: String,
    selector: String,
    description: String,
    input: Value,
    output: Option<Value>,
}

struct Generated {
    files: Arc<BTreeMap<String, String>>,
    modules: Arc<BTreeMap<String, String>>,
    servers: Vec<ServerSummary>,
    bytes: usize,
}

#[derive(Default)]
struct Cache {
    entries: VecDeque<(String, Arc<Generated>)>,
    bytes: usize,
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

pub fn digest(value: &str) -> String {
    hex(&Sha256::digest(value.as_bytes()))
}

/// sha2 0.11 的 digest/finalize 输出不再实现 LowerHex，手动转小写十六进制。
fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

fn identifier(value: &str) -> String {
    // Prefix makes reserved words and leading digits safe without a keyword list.
    let mut out = String::from("tool_");
    out.extend(value.chars().take(80).map(|c| {
        if c.is_ascii_alphanumeric() || c == '_' {
            c
        } else {
            '_'
        }
    }));
    out
}

fn comment(value: &str) -> String {
    value
        .chars()
        .take(2048)
        .collect::<String>()
        .replace("*/", "* /")
        .replace(['\r', '\n'], " ")
}

fn documentation(value: &str) -> String {
    let mut lines = vec![String::from("/**")];
    let mut line = String::from(" * ");
    for word in comment(value).split_whitespace() {
        if line.chars().count() + word.chars().count() > 120 {
            lines.push(line);
            line = String::from(" * ");
        }
        line.push_str(word);
        line.push(' ');
    }
    lines.push(line);
    lines.push(String::from(" */"));
    lines.join("\n")
}

impl Catalog {
    pub fn build(tools: &[Arc<dyn AgentTool>]) -> Result<Self> {
        let mut records = Vec::new();
        let mut callable = BTreeMap::new();
        let mut raw_bytes = 0usize;
        for tool in tools {
            let (kind, server_id, server, original) = match tool.selector() {
                ToolSelector::Mcp {
                    server_id,
                    tool: original,
                } => {
                    ensure!(
                        !server_id.is_empty(),
                        "MCP tool has no stable server identity"
                    );
                    (
                        "mcp",
                        server_id,
                        tool.mcp_server_name().unwrap_or("mcp").to_owned(),
                        original,
                    )
                }
                ToolSelector::Builtin { name }
                    if crate::agent_settings::BUILTIN_TOOLS.contains(&name.as_str())
                        || name == "task_stop" =>
                {
                    ("builtin", String::new(), "builtin".to_owned(), name)
                }
                _ => continue,
            };
            let selector = serde_json::to_string(&tool.selector())?;
            ensure!(
                selector.len() <= oct_codemode::MAX_SELECTOR,
                "Tool selector exceeds code-mode limit"
            );
            ensure!(
                callable.insert(selector.clone(), tool.clone()).is_none(),
                "Duplicate code-mode tool selector"
            );
            let input = tool.input_schema();
            let output = tool.output_schema();
            raw_bytes = raw_bytes
                .saturating_add(serde_json::to_vec(&input)?.len())
                .saturating_add(serde_json::to_vec(&output)?.len());
            ensure!(
                raw_bytes <= CATALOG_BYTES,
                "Tool schemas exceed catalog generation budget"
            );
            records.push(Descriptor {
                kind,
                server_id,
                server,
                original,
                selector,
                description: comment(tool.description()),
                input,
                output,
            });
        }
        records.sort_by(|a, b| {
            (a.kind, &a.server_id, &a.original).cmp(&(b.kind, &b.server_id, &b.original))
        });
        let mut hash = Sha256::new();
        hash.update(GENERATOR);
        hash.update(oct_codemode::PROTOCOL_VERSION.to_be_bytes());
        hash.update(RUNTIME_TYPES);
        hash.update(README);
        hash.update(serde_json::to_vec(&records)?);
        let fingerprint = hex(&hash.finalize());
        let hit = {
            let mut cache = cache().lock().unwrap_or_else(|p| p.into_inner());
            let found = cache
                .entries
                .iter()
                .position(|(key, _)| key == &fingerprint);
            found.map(|index| {
                let entry = cache.entries.remove(index).unwrap();
                let generated = entry.1.clone();
                cache.entries.push_back(entry);
                generated
            })
        };
        let generated = match hit {
            Some(generated) => generated,
            None => {
                let generated = Arc::new(generate(&records, &fingerprint)?);
                let mut cache = cache().lock().unwrap_or_else(|p| p.into_inner());
                while cache.entries.len() >= 8 || cache.bytes + generated.bytes > CACHE_BYTES {
                    let Some((_, old)) = cache.entries.pop_front() else {
                        break;
                    };
                    cache.bytes -= old.bytes;
                }
                // Concurrent misses may create the same catalog; keep just one entry.
                if !cache.entries.iter().any(|(key, _)| key == &fingerprint)
                    && generated.bytes <= CACHE_BYTES
                {
                    cache.bytes += generated.bytes;
                    cache
                        .entries
                        .push_back((fingerprint.clone(), generated.clone()));
                }
                generated
            }
        };
        Ok(Self {
            fingerprint,
            files: generated.files.clone(),
            modules: generated.modules.clone(),
            servers: generated.servers.clone(),
            tools: callable,
        })
    }

    pub async fn publish(&self, sdk: &Path) -> Result<()> {
        let staging = sdk.with_extension(format!("staging-{}", uuid::Uuid::new_v4()));
        let result: Result<()> = async {
            tokio::fs::create_dir_all(&staging).await?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                tokio::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o700))
                    .await?;
            }
            for (path, source) in self.files.iter() {
                let path = staging.join(path);
                tokio::fs::create_dir_all(path.parent().unwrap()).await?;
                tokio::fs::write(&path, source).await?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400))
                        .await?;
                }
            }
            tokio::fs::rename(&staging, sdk).await?;
            Ok(())
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_dir_all(&staging).await;
        }
        result
    }
}

fn generate(records: &[Descriptor], fingerprint: &str) -> Result<Generated> {
    let mut files = BTreeMap::new();
    let mut modules = BTreeMap::new();
    let mut groups: BTreeMap<(&str, &str), Vec<&Descriptor>> = BTreeMap::new();
    for record in records {
        groups
            .entry((record.kind, &record.server_id))
            .or_default()
            .push(record);
    }
    let mut index = Vec::new();
    let mut servers = Vec::new();
    let mut stems = BTreeSet::new();
    for ((kind, server_id), records) in groups {
        let server = &records[0].server;
        let slug = identifier(server).trim_start_matches("tool_").to_owned();
        let stem = if kind == "builtin" {
            "tools".to_owned()
        } else {
            format!("{slug}__{}", &digest(server_id)[..12])
        };
        ensure!(
            stems.insert(stem.clone()),
            "MCP server module name collision"
        );
        let specifier = format!("./{kind}/{stem}.js");
        let mut used = BTreeSet::new();
        let mut entries = Vec::new();
        let mut binding = String::from("import { callTool } from '../_runtime.js';\n");
        for record in &records {
            let mut alias = identifier(&record.original);
            if !used.insert(alias.clone()) {
                alias.push_str(&format!("_{}", digest(&record.original)));
                ensure!(used.insert(alias.clone()), "Duplicate MCP tool identity");
            }
            let declaration = format!(
                "{}\n// Original name: {}\nexport type {alias}Input = {};\nexport type {alias}Output = {};\nexport declare function {alias}(input: {alias}Input): Promise<ToolResult<{alias}Output>>;\n\n",
                documentation(&record.description),
                comment(&record.original),
                render(&record.input),
                record
                    .output
                    .as_ref()
                    .map(render)
                    .unwrap_or_else(|| "unknown".into())
            );
            binding.push_str(&format!(
                "export const {alias} = input => callTool({}, input);\n",
                serde_json::to_string(&record.selector)?
            ));
            entries.push((alias, declaration, *record));
        }
        let split = entries.len() > SPLIT_TOOLS
            || entries.iter().map(|(_, d, _)| d.len()).sum::<usize>() > SPLIT_BYTES;
        let declaration_path = if split {
            format!("{kind}/{stem}/index.d.ts")
        } else {
            format!("{kind}/{stem}.d.ts")
        };
        let schema_path = if split {
            format!("schemas/{kind}/{stem}/index.json")
        } else {
            format!("schemas/{kind}/{stem}.json")
        };
        let runtime_import = if split {
            "../../_runtime.js"
        } else {
            "../_runtime.js"
        };
        let mut declarations = format!(
            "// importSpecifier: {specifier}\nimport type {{ ToolResult }} from '{runtime_import}';\n\n"
        );
        let mut tool_index = Vec::new();
        let mut schemas = BTreeMap::new();
        for (alias, declaration, record) in entries {
            let raw = json!({"name":record.original,"inputSchema":record.input,"outputSchema":record.output});
            let (tool_path, tool_schema) = if split {
                let path = format!("{kind}/{stem}/{alias}.d.ts");
                let schema = format!("schemas/{kind}/{stem}/{alias}.json");
                files.insert(path.clone(),format!("// importSpecifier: {specifier}\n// schema: {schema}\nimport type {{ ToolResult }} from '../../_runtime.js';\n\n{declaration}"));
                files.insert(schema.clone(), serde_json::to_string_pretty(&raw)?);
                let summary = record.description.chars().take(140).collect::<String>();
                declarations.push_str(&format!(
                    "// {}: {summary} | schema: {schema}\nexport * from './{alias}.js';\n",
                    comment(&record.original)
                ));
                modules.insert(
                    format!("{kind}/{stem}/{alias}.js"),
                    format!("export {{ {alias} }} from '../{stem}.js';\n"),
                );
                schemas.insert(alias.clone(), json!({"name":record.original,"path":schema}));
                (path, schema)
            } else {
                declarations.push_str(&declaration);
                schemas.insert(alias.clone(), raw);
                (declaration_path.clone(), schema_path.clone())
            };
            tool_index.push(json!({"name":record.original,"export":alias,"declarations":tool_path,"schema":tool_schema,"description":record.description.chars().take(140).collect::<String>()}));
        }
        let tool_index_path = format!("{kind}/{stem}.tools.json");
        files.insert(
            tool_index_path.clone(),
            serde_json::to_string_pretty(&tool_index)?,
        );
        files.insert(declaration_path.clone(), declarations);
        files.insert(schema_path.clone(), serde_json::to_string_pretty(&schemas)?);
        modules.insert(format!("{kind}/{stem}.js"), binding);
        servers.push(ServerSummary {
            kind: records[0].kind,
            server: server.clone(),
            tools: records.len(),
        });
        index.push(json!({"kind":kind,"server":server,"declarations":declaration_path,"schemas":schema_path,"importSpecifier":specifier,"toolIndex":tool_index_path,"tools":records.len(),"split":split}));
        ensure!(
            files.values().map(String::len).sum::<usize>() <= CATALOG_BYTES,
            "Tool catalog exceeds generation budget"
        );
    }
    files.insert("index.json".into(), serde_json::to_string_pretty(&index)?);
    files.insert("_runtime.d.ts".into(), RUNTIME_TYPES.into());
    files.insert("README.md".into(), README.into());
    let hashes: BTreeMap<_, _> = files.iter().map(|(p, s)| (p, digest(s))).collect();
    files.insert(
        "catalog.json".into(),
        serde_json::to_string_pretty(
            &json!({"generator":GENERATOR,"fingerprint":fingerprint,"files":hashes}),
        )?,
    );
    modules.insert("_runtime.js".into(),"export const callTool = globalThis.__oct_call; export const text = globalThis.text; export const image = globalThis.image;".into());
    let file_bytes = files.values().map(String::len).sum::<usize>();
    let module_bytes = serde_json::to_vec(&modules)?.len();
    ensure!(
        file_bytes <= CATALOG_BYTES,
        "Tool catalog exceeds generation budget"
    );
    ensure!(
        files.values().all(|s| s.len() <= 4 * 1024 * 1024),
        "Tool schema or index exceeds file budget"
    );
    ensure!(
        module_bytes <= oct_codemode::MAX_FRAME - 1024 * 1024,
        "Tool virtual modules exceed code-mode initialization budget"
    );
    Ok(Generated {
        files: Arc::new(files),
        modules: Arc::new(modules),
        servers,
        bytes: file_bytes + module_bytes,
    })
}

const RUNTIME_TYPES: &str = r#"export interface ToolResult<T = unknown> {
  text: string;
  structuredContent?: T;
  images: Array<{mimeType: string; data: string}>;
  isError: boolean;
}
export interface ToolCallError extends Error {
  name: 'ToolCallError';
  code: string;
  result?: ToolResult;
}
export declare function text(value: unknown): void;
export declare function image(value: {mimeType: string; data: string}): void;
declare global {
  function text(value: unknown): void;
  function image(value: {mimeType: string; data: string}): void;
  const console: {log(...values: unknown[]): void; info(...values: unknown[]): void; warn(...values: unknown[]): void; error(...values: unknown[]): void};
}
"#;

const README: &str = "# Code mode SDK\n\nRead index.json for the module list and exact import specifiers, then grep/read the .d.ts declarations with the authorized file tools (read_file/grep/glob); this README explains the execution rules. The builtin module contains only authorized file/bash tools; MCP modules are grouped by server. The SDK is a read-only root: reads grant no edit rights and writes are denied. Large servers have a short index.d.ts and one declaration/schema file per tool; toolIndex lists exact names, exports and paths. Import the exact importSpecifier from index.json in execute_code. Relative imports start at the SDK root, even though cell source is saved elsewhere. .d.ts files describe interfaces and cannot be executed; .js modules are supplied by the host and are not on disk.\n\nThe schemas path in index.json points to the original inputSchema/outputSchema. Read it when a declaration is unknown or a long signature was clipped by file preview. Types are approximations; TypeScript is transpiled without type checking, and the host validates the original input schema.\n\nCode is a TypeScript ES module with top-level await. Await every tool call. Use Promise.all / allSettled for independent work. Every builtin/MCP call uses normal approvals and can throw ToolCallError. Native results retain their normal bounded text previews (read_file includes line numbers); request smaller ranges when needed. Direct native calls and code calls share this agent's read state and workspace locks. Workers have independent read state and cannot start background commands; task is only an outer tool. Inspect error.code (invalid_arguments, permission_denied, tool_error, tool_timeout, transport_error, result_too_large) and error.result for available MCP failure data. Results include text, optional structuredContent and images; text is not implicitly parsed as JSON. Emit only needed information with text(value) or console.log(value); emit an image explicitly with image(result.images[0]). Intermediate results stay in the script. No direct filesystem, network, subprocess, npm or arbitrary imports. Cancellation does not roll back remote actions.\n";

/// Bounded rendering. Runtime validation always uses the original schema.
pub fn render(schema: &Value) -> String {
    let out = ty(schema, schema, 0, &mut 512);
    if out.len() > 16 * 1024 {
        "unknown".into()
    } else {
        out
    }
}

fn ty(value: &Value, root: &Value, depth: usize, budget: &mut usize) -> String {
    if *budget == 0 {
        return "unknown".into();
    }
    *budget -= 1;
    if depth > 10 || value == &Value::Bool(true) {
        return "unknown".into();
    }
    if value == &Value::Bool(false) {
        return "never".into();
    }
    if let Some(reference) = value.get("$ref").and_then(Value::as_str) {
        return reference
            .strip_prefix('#')
            .and_then(|p| root.pointer(p))
            .map(|v| ty(v, root, depth + 1, budget))
            .unwrap_or_else(|| "unknown".into());
    }
    if let Some(constant) = value.get("const") {
        return literal(constant);
    }
    if let Some(values) = value.get("enum").and_then(Value::as_array) {
        if values.is_empty() {
            return "never".into();
        }
        if values.len() > 64 {
            return "unknown".into();
        }
        return values
            .iter()
            .map(literal)
            .collect::<Vec<_>>()
            .join(&format!(" |\n{}", "  ".repeat(depth)));
    }
    for (key, separator) in [("oneOf", " | "), ("anyOf", " | "), ("allOf", " & ")] {
        if let Some(values) = value.get(key).and_then(Value::as_array) {
            if values.is_empty() {
                return if key == "allOf" { "unknown" } else { "never" }.into();
            }
            if values.len() > 32 {
                return "unknown".into();
            }
            return format!(
                "({})",
                values
                    .iter()
                    .map(|v| ty(v, root, depth + 1, budget))
                    .collect::<Vec<_>>()
                    .join(separator)
            );
        }
    }
    if let Some(types) = value.get("type").and_then(Value::as_array) {
        if types.len() > 8 {
            return "unknown".into();
        }
        return types
            .iter()
            .map(|t| {
                let mut v = value.clone();
                v["type"] = t.clone();
                ty(&v, root, depth + 1, budget)
            })
            .collect::<Vec<_>>()
            .join(" | ");
    }
    let result = match value
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| value.get("properties").map(|_| "object"))
    {
        Some("string") => "string".into(),
        Some("integer" | "number") => "number".into(),
        Some("boolean") => "boolean".into(),
        Some("null") => "null".into(),
        Some("array") => format!(
            "Array<{}>",
            value
                .get("items")
                .map(|v| ty(v, root, depth + 1, budget))
                .unwrap_or_else(|| "unknown".into())
        ),
        Some("object") => {
            let required = value.get("required").and_then(Value::as_array);
            let mut fields = Vec::new();
            if let Some(props) = value.get("properties").and_then(Value::as_object) {
                for (name, field) in props.iter().take(256) {
                    let note = field
                        .get("description")
                        .and_then(Value::as_str)
                        .map(|s| format!("{}\n{}", documentation(s), "  ".repeat(depth + 1)))
                        .unwrap_or_default();
                    let optional =
                        if required.is_some_and(|r| r.iter().any(|n| n.as_str() == Some(name))) {
                            ""
                        } else {
                            "?"
                        };
                    fields.push(format!(
                        "{note}{}{optional}: {}",
                        serde_json::to_string(name).unwrap(),
                        ty(field, root, depth + 1, budget)
                    ));
                }
                if props.len() > 256 {
                    fields.push("[key: string]: unknown".into());
                }
            }
            if value.get("additionalProperties") != Some(&Value::Bool(false)) {
                fields.push("[key: string]: unknown".into());
            }
            let indent = "  ".repeat(depth + 1);
            format!(
                "{{\n{}\n{}}}",
                fields
                    .iter()
                    .map(|f| format!("{indent}{f};"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                "  ".repeat(depth)
            )
        }
        _ => "unknown".into(),
    };
    if value.get("nullable") == Some(&Value::Bool(true)) {
        format!("({result}) | null")
    } else {
        result
    }
}

fn literal(value: &Value) -> String {
    if value.is_object() || value.is_array() || value.as_str().is_some_and(|s| s.len() > 1024) {
        "unknown".into()
    } else {
        value.to_string()
    }
}
