//! One isolate per process. Stdio is held by Rust, never added to V8's resources.
use anyhow::{Context, Result};
use deno_core::error::ModuleLoaderError;
use deno_core::{
    JsRuntime, ModuleLoadOptions, ModuleLoadReferrer, ModuleLoadResponse, ModuleLoader,
    ModuleSource, ModuleSourceCode, ModuleSpecifier, ModuleType, OpState, ResolutionKind,
    RuntimeOptions, op2,
};
use deno_error::JsErrorBox;
use oct_codemode::protocol::*;
use serde_json::Value;
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::rc::Rc;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::oneshot;

struct Bridge {
    writer: Mutex<std::io::Stdout>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, CallError>>>>,
    seq: AtomicU64,
    emitted: AtomicU64,
}

impl Bridge {
    fn send(&self, message: &FromWorker) -> Result<()> {
        let data = serde_json::to_vec(message)?;
        anyhow::ensure!(data.len() <= MAX_FRAME, "Code mode frame exceeds limit");
        let mut writer = self.writer.lock().unwrap();
        writer.write_all(&(data.len() as u32).to_be_bytes())?;
        writer.write_all(&data)?;
        writer.flush()?;
        Ok(())
    }
}

fn read_sync(reader: &mut impl Read) -> Result<ToWorker> {
    let mut size = [0; 4];
    reader.read_exact(&mut size)?;
    let size = u32::from_be_bytes(size) as usize;
    anyhow::ensure!(size <= MAX_FRAME, "Code mode frame exceeds limit");
    let mut body = vec![0; size];
    reader.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

#[op2]
#[serde]
async fn op_oct_call(
    state: Rc<RefCell<OpState>>,
    #[string] tool: String,
    #[serde] args: serde_json::Value,
) -> Result<serde_json::Value, JsErrorBox> {
    let bridge = state.borrow().borrow::<Arc<Bridge>>().clone();
    if let Err(error) = call_payload_size(&tool, &args) {
        return Ok(
            serde_json::json!({"ok":false,"error":CallError::new("invalid_arguments",error.to_string())}),
        );
    }
    let seq = bridge.seq.fetch_add(1, Ordering::SeqCst) + 1;
    if seq > MAX_CALLS {
        return Ok(
            serde_json::json!({"ok":false,"error":CallError::new("call_limit","Code mode call budget exceeded")}),
        );
    }
    let (tx, rx) = oneshot::channel();
    bridge.pending.lock().unwrap().insert(seq, tx);
    bridge
        .send(&FromWorker::Call { seq, tool, args })
        .map_err(|error| JsErrorBox::generic(error.to_string()))?;
    let result = rx
        .await
        .map_err(|error| JsErrorBox::generic(error.to_string()))?;
    // Return failure as data; the public JS wrapper creates a catchable error.
    Ok(match result {
        Ok(value) => serde_json::json!({"ok":true,"value":value}),
        Err(error) => serde_json::json!({"ok":false,"error":error}),
    })
}

#[op2(fast)]
fn op_oct_emit(state: Rc<RefCell<OpState>>, #[string] text: String) -> Result<(), JsErrorBox> {
    let bridge = state.borrow().borrow::<Arc<Bridge>>().clone();
    if bridge
        .emitted
        .fetch_add(text.len() as u64, Ordering::SeqCst)
        + text.len() as u64
        > MAX_OUTPUT as u64
    {
        return Err(JsErrorBox::generic("Code mode output exceeds limit"));
    }
    bridge
        .send(&FromWorker::Emit { text })
        .map_err(|error| JsErrorBox::generic(error.to_string()))
}

#[op2(fast)]
fn op_oct_image(
    state: Rc<RefCell<OpState>>,
    #[string] mime_type: String,
    #[string] data: String,
) -> Result<(), JsErrorBox> {
    let bridge = state.borrow().borrow::<Arc<Bridge>>().clone();
    if data.len() > 5 * 1024 * 1024 {
        return Err(JsErrorBox::generic("Image exceeds size limit"));
    }
    bridge
        .send(&FromWorker::Image { mime_type, data })
        .map_err(|error| JsErrorBox::generic(error.to_string()))
}

deno_core::extension!(oct_bridge, ops = [op_oct_call, op_oct_emit, op_oct_image]);

struct Loader {
    modules: BTreeMap<String, String>,
    maps: RefCell<HashMap<String, Vec<u8>>>,
}

impl ModuleLoader for Loader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        kind: ResolutionKind,
    ) -> Result<ModuleSpecifier, ModuleLoaderError> {
        if matches!(kind, ResolutionKind::MainModule) && specifier == "oct://sdk/main.ts" {
            return ModuleSpecifier::parse(specifier)
                .map_err(|error| JsErrorBox::generic(error.to_string()));
        }
        if !specifier.starts_with("./") && !specifier.starts_with("../") {
            return Err(JsErrorBox::generic(
                "Only relative SDK module imports are allowed",
            ));
        }
        let resolved = deno_core::resolve_import(specifier, referrer)
            .map_err(|error| JsErrorBox::generic(error.to_string()))?;
        if !self.modules.contains_key(resolved.as_str()) {
            return Err(JsErrorBox::generic(format!(
                "Unknown SDK module: {specifier}. Read index.json for importSpecifier; .d.ts files cannot be executed."
            )));
        }
        Ok(resolved)
    }

    fn load(
        &self,
        specifier: &ModuleSpecifier,
        _: Option<&ModuleLoadReferrer>,
        _: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        let loaded = (|| {
            let source = self
                .modules
                .get(specifier.as_str())
                .ok_or_else(|| JsErrorBox::generic("Module is outside the SDK snapshot"))?;
            let source = if specifier.path().ends_with(".ts") {
                let parsed = deno_ast::parse_module(deno_ast::ParseParams {
                    specifier: specifier.clone(),
                    text: source.clone().into(),
                    media_type: deno_ast::MediaType::TypeScript,
                    capture_tokens: false,
                    scope_analysis: false,
                    maybe_syntax: None,
                })
                .map_err(|error| JsErrorBox::generic(error.to_string()))?;
                let emitted = parsed
                    .transpile(
                        &deno_ast::TranspileOptions {
                            imports_not_used_as_values: deno_ast::ImportsNotUsedAsValues::Remove,
                            ..Default::default()
                        },
                        &deno_ast::TranspileModuleOptions { module_kind: None },
                        &deno_ast::EmitOptions {
                            source_map: deno_ast::SourceMapOption::Separate,
                            inline_sources: true,
                            ..Default::default()
                        },
                    )
                    .map_err(|error| JsErrorBox::generic(error.to_string()))?
                    .into_source();
                if let Some(map) = emitted.source_map {
                    self.maps
                        .borrow_mut()
                        .insert(specifier.to_string(), map.into_bytes());
                }
                emitted.text
            } else {
                source.clone()
            };
            Ok(ModuleSource::new(
                ModuleType::JavaScript,
                ModuleSourceCode::String(source.into()),
                specifier,
                None,
            ))
        })();
        ModuleLoadResponse::Sync(loaded)
    }

    fn get_source_map(&self, specifier: &str) -> Option<Cow<'_, [u8]>> {
        self.maps.borrow().get(specifier).cloned().map(Cow::Owned)
    }
}

const BOOTSTRAP: &str = r#"
(() => {
    const { op_oct_call, op_oct_emit, op_oct_image } = Deno.core.ops;
    const stringify = JSON.stringify;
    const NativeError = Error;
    const define = (name, value) => Object.defineProperty(globalThis, name, { value, writable: false, configurable: false });
    define('__oct_call', async (tool, args) => {
        try {
            const result = await op_oct_call(tool, args);
            if (!result.ok) {
                const error = new NativeError(result.error.message);
                error.code = result.error.code;
                if (result.error.result !== undefined) error.result = result.error.result;
                throw error;
            }
            return result.value;
        } catch (cause) {
            const error = new NativeError(cause.message ?? String(cause));
            error.name = 'ToolCallError';
            error.code = cause.code ?? 'runtime_error';
            if (cause.result !== undefined) error.result = cause.result;
            throw error;
        }
    });
    const text = value => op_oct_emit((typeof value === 'string' ? value : stringify(value)) + '\n');
    define('text', text);
    define('image', value => op_oct_image(value.mimeType, value.data));
    Object.defineProperty(globalThis, 'console', {value: Object.freeze({log: (...args) => args.forEach(text), info: (...args) => args.forEach(text), warn: (...args) => args.forEach(text), error: (...args) => args.forEach(text)}), writable: false, configurable: false});
    for (const name of ['Deno', 'WebAssembly', 'SharedArrayBuffer', 'Atomics']) {
        delete globalThis[name];
        define(name, undefined);
    }
})();
"#;

async fn execute(init: Init, bridge: Arc<Bridge>) -> Result<()> {
    anyhow::ensure!(init.code.len() <= MAX_CODE, "Code input exceeds limit");
    let root = ModuleSpecifier::parse("oct://sdk/")?;
    let mut modules = BTreeMap::new();
    for (path, source) in init.modules {
        let url = root.join(&path)?;
        anyhow::ensure!(
            url.as_str().starts_with(root.as_str()) && path.ends_with(".js"),
            "Invalid SDK module path"
        );
        modules.insert(url.to_string(), source);
    }
    let main = root.join("main.ts")?;
    modules.insert(main.to_string(), init.code);
    let loader = Rc::new(Loader {
        modules,
        maps: RefCell::new(HashMap::new()),
    });
    let mut runtime = JsRuntime::new(RuntimeOptions {
        extensions: vec![oct_bridge::init()],
        module_loader: Some(loader),
        create_params: Some(
            deno_core::v8::CreateParams::default().heap_limits(0, 256 * 1024 * 1024),
        ),
        ..Default::default()
    });
    runtime.op_state().borrow_mut().put(bridge.clone());
    // Initialize core's cached callbacks before removing its public namespace.
    runtime.run_event_loop(Default::default()).await?;
    runtime.execute_script("oct:bootstrap", BOOTSTRAP)?;
    let module = runtime.load_main_es_module(&main).await?;
    let evaluated = runtime.mod_evaluate(module);
    runtime
        .with_event_loop_promise(evaluated, Default::default())
        .await?;
    anyhow::ensure!(
        bridge.pending.lock().unwrap().is_empty(),
        "unawaited_tool_calls: await every tool call; remote actions may have taken effect"
    );
    // Poll once more for unhandled rejections left by the module.
    runtime.run_event_loop(Default::default()).await?;
    Ok(())
}

fn main() -> Result<()> {
    // Only Rust writes stdout protocol frames; JS output uses bridge ops.
    let bridge = Arc::new(Bridge {
        writer: Mutex::new(std::io::stdout()),
        pending: Mutex::new(HashMap::new()),
        seq: AtomicU64::new(0),
        emitted: AtomicU64::new(0),
    });
    bridge.send(&FromWorker::Hello {
        protocol_version: PROTOCOL_VERSION,
    })?;
    let ToWorker::Init(init) = read_sync(&mut std::io::stdin().lock())? else {
        anyhow::bail!("Missing code mode init");
    };
    let read_bridge = bridge.clone();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        while let Ok(ToWorker::Reply { seq, result }) = read_sync(&mut reader) {
            if let Some(tx) = read_bridge.pending.lock().unwrap().remove(&seq) {
                let _ = tx.send(result);
            }
        }
        // Backend closed stdin: terminate even if V8 is spinning.
        std::process::exit(1);
    });
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(execute(init, bridge.clone()));
    bridge
        .send(&FromWorker::Done {
            error: result
                .err()
                .map(|e| format!("{e:#}").chars().take(8192).collect()),
        })
        .context("Cannot report code mode result")?;
    Ok(())
}
