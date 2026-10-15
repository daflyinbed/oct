# oct-llm-provider

LLM provider 抽象层 crate：统一的 ChatModel/Embedding 接口、provider 注册表、OpenAI 兼容 / Anthropic 适配器与 LLM wire 日志。oct-agent 通过它消费所有模型调用，不直接依赖任何厂商 SDK。

## 模块

- `core/` — 中性类型：消息（`message.rs`）、工具（`tool.rs`）、流（`stream.rs`）、usage、options、错误
- `model/` — `ChatModel`（单次 `generate` + 流式 `stream`）与 embedding 接口
- `provider/` — `Provider` trait、`ProviderRegistry`（按 `provider:model_id` spec 解析模型；`resolve_custom_openai_model` 为运行时注册的自定义 OpenAI 兼容 provider 现建 ChatModel）、capabilities/limits/model_info
- `adapter/` — 内部类型 ↔ provider JSON（`openai_compatible/`；Anthropic 侧在 `providers/anthropic`）
- `providers/` — 具体注册：Anthropic、MoonshotAI（Kimi）
- `transport.rs` — reqwest 传输（工作区统一 reqwest 0.13）
- `wire.rs` + `wire/retention.rs` — LLM wire 日志（见下）

Anthropic 的签名思考块与脱敏块携带本地请求前缀指纹，随消息保存但不发送给模型。对于执行前缀绑定的模型，code mode SDK 路径、指令、工具或历史变化后，适配器仅在请求副本中过滤失效块；前缀不变的工具循环继续原样续传，数据库保留原始历史。旧记录缺少指纹时在这些模型上保守省略思考块；旧模型的手动思考续传保持兼容。实现见 `providers/anthropic/replay.rs`。

## LLM wire 日志

每个 LLM HTTP 交互的原始 wire 记录：精确请求体 + 每个 framing SSE 事件块 + 流结束/错误，一条一事件的 JSONL，按对话拆文件 `{db目录}/llm-wire/{conversation_id}.jsonl`（无会话上下文落 `_orphan.jsonl`）。单文件 16 MiB 轮转，目录总量 256 MiB；单字段默认 1 MiB 截断标 `truncated`。

开关与位置：

- 设置页 AI 根页（`GET/PUT /api/settings` 的 `llm_wire_log`，持久化在 `settings` 表、运行时即时生效，默认开）
- `OCT_LLM_WIRE_LOG=off` 进程级硬关；`OCT_LLM_WIRE_DIR` 改目录；`OCT_LLM_WIRE_FIELD_MAX=0` 关字段截断
- stderr 镜像：`RUST_LOG="oct_llm_provider::wire=trace"`

实现要点（改 `wire.rs` 前先读 `AGENTS.md` 的埋点与 tracing layer 陷阱）。

## 测试

- **insta 快照**（YAML）：fixtures 在 `tests/<provider>/fixtures/`；有意变更后 `cargo insta review` 接受新快照。文件：`adapter_openai_compatible.rs`、`providers_anthropic.rs`、`registry.rs`、`anthropic/`、`moonshot/`
- **wire 格式测试**（`tests/http_wire.rs`）：raw-TCP mock server（`tests/http_mock/`）驱动真实 reqwest + SSE 解码路径，响应体按"恶意"字节边界切块，覆盖 401/429/500 状态映射
- **wire layer 行为**：`tests/wire_layer_filter.rs` / `tests/wire_toggle.rs`

CI 铁律：测试绝不调用真实模型，API key 经环境变量注入（Anthropic）或显式参数注入（OpenAI）。
