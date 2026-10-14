# oct-llm-provider AGENTS.md

- **Provider abstraction**: `Provider` trait creates `ChatModel`. `ProviderRegistry` resolves models by `provider:model_id` spec and supports dynamic custom OpenAI-compatible providers via `resolve_custom_openai_model()`. Adapters (`adapter/openai_compatible.rs`) convert internal types to provider-specific JSON. Concrete providers: Anthropic, MoonshotAI (Kimi).
- **Provider registry** (`provider/registry.rs`): `ProviderRegistry` holds registered providers, resolves chat model specs, and can create ad-hoc OpenAI-compatible chat models for custom providers at runtime.
- reqwest 全仓统一 0.13（与 oct-mcp 的 rmcp reqwest 对齐）。

## LLM wire 日志

- 每个 LLM HTTP 交互的原始 wire 记录（精确请求体 + 每个 framing SSE 事件块 + 流结束/错误），一条一事件的 JSONL，**按对话拆文件**：`{db目录}/llm-wire/{conversation_id}.jsonl`（无会话上下文落 `_orphan.jsonl`；0600；单文件 16 MiB 轮转（保留一份 .1），目录总量 256 MiB，超限淘汰最旧诊断记录；单字段默认 1 MiB 截断标 `truncated`，`OCT_LLM_WIRE_FIELD_MAX=0` 关闭）。
- 开关：设置弹窗 AI 根页（`GET/PUT /api/settings` 的 `llm_wire_log`，持久化在 `settings` 表、运行时即时生效，默认开）；`OCT_LLM_WIRE_LOG=off` 进程级硬关；`OCT_LLM_WIRE_DIR` 改目录。stderr 镜像：`RUST_LOG="oct_llm_provider::wire=trace"`。
- 埋点在 `src/wire.rs`（target `oct_llm_provider::wire`；相关性字段用 task-local tags——host 侧 `wire::with_tags` 包住模型调用，`try_stream!` 生成器由消费方 poll、不在 scope 内，故 `stream()` 入口捕获进 `WireCtx`）。注意 `Layer::register_callsite` 必须返回 `Interest::sometimes()`：默认实现会在开关关闭时缓存 `never`，毒化 callsite 导致再开也不记录；同理 `Layer::enabled` 是**全局收窄语义**——非 wire target 必须答 `true`（答 `false` 会短路整个订阅者栈、连 fmt 层的控制台输出一起杀掉），仅 wire target 以 sink 开关作答（兼任 adapter 侧 `tracing::enabled!` 序列化探针），层级/开关过滤放 `on_event`。

## 测试

- CI 铁律：测试绝不调用真实模型；API key 经环境变量注入（Anthropic）或显式参数注入（OpenAI）。
- **insta** 快照测试（YAML）。fixtures 是 `tests/<provider>/fixtures/` 下的 JSON。有意变更输出后跑 `cargo insta review` 接受新快照。测试文件：`adapter_openai_compatible.rs`、`providers_anthropic.rs`、`registry.rs`、`anthropic/`、`moonshot/`。
- **wire 格式测试**（`tests/http_wire.rs`）: raw-TCP mock server（`tests/http_mock/`）驱动真实 reqwest + SSE 解码路径，响应体按"恶意"字节边界切块（多字节 UTF-8 中间、SSE 事件中间、data 行中间），并覆盖 401/429/500 状态映射。
- wire layer 行为：`tests/wire_layer_filter.rs` / `tests/wire_toggle.rs`。
