# AGENTS.md

## Monorepo Layout

Two separate package managers coexist:

- **Rust workspace** (`Cargo.toml`): `crates/oct-llm-provider` (library) → `crates/oct-mcp` (MCP client quarantine crate: wraps rmcp behind a narrow `McpConnection` trait; stdio transport only, no reqwest in its dependency tree) → `crates/oct-agent` (library + `oct-agent` binary; integration tests import `oct_agent::…`)
- **pnpm workspace** (`pnpm-workspace.yaml`): `packages/app` (Vue 3 frontend)

## Commands

### Rust (run from workspace root)

```bash
cargo build -p oct-agent              # build the backend binary
cargo run -p oct-agent                 # run backend `serve` subcommand (port 3000 by default, override with OCT_PORT)
cargo run -p oct-agent -- serve        # explicit serve subcommand
cargo run -p oct-agent -- seed         # seed providers & models from models.dev API into the database
cargo test -p oct-llm-provider         # run library tests (snapshot tests via insta)
cargo test -p oct-llm-provider --test anthropic  # single integration test file
cargo sqlx prepare --workspace         # regenerate .sqlx/ cache
```

### Frontend (run from workspace root with pnpm)

```bash
pnpm --filter frontend dev            # Vite dev server (port 5173)
pnpm --filter frontend build          # vue-tsc typecheck then vite build
pnpm --filter frontend generate-api   # regenerate TypeScript types from OpenAPI schema (requires running server)
pnpm --filter frontend test           # vitest 单测（独立 vitest.config.ts，node 环境，不加载 vue/unocss 插件）
pnpm --filter frontend test:browser  # 浏览器 e2e 拦截层（chromium；首次需 `pnpm --filter frontend exec playwright install chromium`）
```

### Frontend unit tests

- vitest 测试与源码同目录（`src/**/*.test.ts`）。测 composable 时优先经公开 API 驱动：mock `@/api/client`（REST）与全局 `fetch`（SSE），不导出私有函数。
- `useChat` 的 SSE mock：用 `ReadableStream` 构造 `Response`，可按任意字符串切块模拟网络分片。

### Frontend browser e2e（拦截层）

- `browser-e2e/` + 独立 `vitest.browser.config.ts`：vitest browser mode（Playwright chromium）在真实浏览器里挂载完整 App（复用 `main.ts` 的装配 + vue/unocss/unplugin 插件链），黑盒驱动 UI 全流程。
- "后端"是剧本化 mock 服务器（`browser-e2e/mock/server.ts`，node:http 零依赖）：配置求值时启动（随机端口 + `unref`），经 Vite dev server 的 `/api` 代理注入——与 dev/preview 同一条代理路径，浏览器走真实 fetch → HTTP → 代理 → SSE。
- 剧本按会话组织（`scripts[convId]`）。步骤：`events`（AgentEvent 数组；`slices`/`cut` 做字节级切块，边界自然落在多字节 UTF-8、`data:` 行、JSON 中间）、`comment`（keep-alive 注释行）、`gate`（命名门：测试经 `/__mock/gates/:name/open` 放行，是流中暂停的同步原语，禁 sleep）、`status`（POST 直接回错误码）、`hang`（挂流，配合 cancel 注入 `cancelled`）。
- 管理 API 同源（`/__mock` 代理）：`reset` / `scenario` / `gates/:name/open` / `requests`（请求日志，断言 cancel 调用、`provider_spec` 等）。
- 约定：fixture 显式对齐 `schema.d.ts` 类型（后端改字段 → 前端这边编译期报错）；断言一律 `expect.poll(chatText)`（`useChat` 每 50ms flush 一次 stream buffer，瞬时断言必 flaky）；**测试文件必须串行**（`fileParallelism: false`，mock 状态是全局单例，并发会互相覆盖剧本）；每个文件独享 iframe（composable 模块级状态按文件隔离）。
- 不启真后端、零真实模型，CI 铁律天然满足。

### Lint

```bash
pnpm -r exec eslint .                 # ESLint with @xwbx/eslint-config flat config
```

## Build & Database

- **Database**: SQLite. Default `DATABASE_URL`: `sqlite://oct.db` (override via env var or `.env`)
- **sqlx compile-time macros**: All queries use `sqlx::query!`/`query_as!`/`query_scalar!` (compile-time checked, `sqlite` feature)
- **Offline mode**: `.sqlx/` cache is committed; builds work without a live database via `SQLX_OFFLINE=true`
- **Regenerate cache**: After changing SQL queries, run migrations against a SQLite database and then run `cargo sqlx prepare --workspace`
- **Migrations**: `sqlx::migrate!("../../migrations")` in `crates/oct-agent/src/db/mod.rs` auto-runs on startup; migration files live at repo root `migrations/`
- **Seeding**: `cargo run -p oct-agent -- seed` fetches the model catalog from `https://models.dev/api.json` and upserts providers/models (source=`seeded`). Custom providers/models (source=`custom`) are preserved.

## CLI Subcommands

`oct-agent` has two subcommands (via `clap`):

1. **`serve`** (default): Start the API server. Options: `--port` / `OCT_PORT` (default 3000)
2. **`seed`**: Fetch providers and models from models.dev and upsert into the database

## Architecture

- **Provider abstraction** (`oct-llm-provider`): `Provider` trait creates `ChatModel`. `ProviderRegistry` resolves models by `provider:model_id` spec and supports dynamic custom OpenAI-compatible providers via `resolve_custom_openai_model()`. Adapters (`adapter/openai_compatible.rs`) convert internal types to provider-specific JSON. Concrete providers: Anthropic, MoonshotAI (Kimi).
- **Provider registry** (`oct-llm-provider/provider/registry.rs`): `ProviderRegistry` holds registered providers, resolves chat model specs, and can create ad-hoc OpenAI-compatible chat models for custom providers at runtime.
- **Agent loop** (`oct-agent/agent/`): Streams LLM responses, executes tool calls, feeds results back. Supports cancellation via `tokio_util::sync::CancellationToken`. No iteration limit.
- **Agent tools** (`oct-agent/tools/`): `read`, `write`, `listdir`, `execute`
- **Context compaction** (`oct-agent/agent/compact.rs` + `db/messages.rs::load_llm_view` + loop 集成): 对话过长时把切割点之前的历史摘要成一条标记行。触发：turn 顶部（每次 LLM 请求前，覆盖 run 首轮与工具轮之间）按「provider 真实 usage 锚点（上一笔请求 input+output）+ 本地增量估算（ASCII÷4 + 非 ASCII×1；无锚点时含 system+工具 schema）」对比窗口 ×0.85，或手动 `/compact`。窗口解析链 `ModelInfo.limits.max_input_tokens` → DB `models.limit_context` → 未知则禁用自动。压缩 = 同模型单轮无工具 `generate`（结构化分段 prompt，输入序列化截断：reasoning 丢弃、tool result 截 1500 字符、总量超限中部丢弃）+ 落一条 role=user 标记行（parts 带摘要框架文本，details_json `kind:"compaction"` 元数据）；**老行永不删除**，`load_llm_view` 只返回「最新标记行之后」的视图并过滤孤儿 ToolResult（repair 也只修标记之后的悬空调用）。尾部 verbatim 保留（窗口/10，封顶 32k，绝不拆 tool_call/tool_result 对）；防死循环（再触发需 est ≥ 压缩后估算 + tail 预算；摘要失败本 run 禁用自动）。SSE 事件 `compacting`/`compacted`（带全量 summary）；`usage` 事件带 `context_window_tokens`；手动端点 `POST /api/conversations/{id}/compact`（session 活动中 409）。前端：`CompactionDivider` 分隔条（stored 标记行 + live 事件两路归一）、`useContextUsage` + 输入框内 `ContextRing` 上下文占用圆环（hover 弹容量详情）、输入框 `/compact` 命令拦截。
- **MCP support** (`oct-mcp` + `oct-agent/src/mcp.rs` + `oct-agent/api/mcp.rs` + `oct-agent/db/mcp.rs`): MCP 客户端 Phase 1（仅 tools 能力、stdio 传输）。`oct-mcp` 隔离 rmcp（stdio-only features，依赖树禁 reqwest，防与 workspace 的 reqwest 0.12 冲突），公开窄接口 `McpConnection`/`connect_stdio`/`qualified_tool_name`（`mcp__{server}__{tool}`，64 字符截断+哈希后缀）/`ToolNameMap`。`McpManager` 进 AppState，每个 chat turn 前按当前项目对账 enabled `mcp_servers` 行（作用域两级合并：`project_id` NULL=全局、非 NULL=仅该项目；增删改/指纹变更重连、out-of-scope 的过期 entry 仅移除不重连、失败记 Failed、best-effort；连接 key 始终是 server id，跨项目/对话复用同一条连接，scope 只过滤建连触发方与 run 工具集），连接的工具以 `McpTool`（实现 `AgentTool`）拼进 run toolset；工具调用超时默认 60s（行级 `tool_timeout_ms` 覆盖）。REST CRUD 在 `/api/mcp/servers`（列表/详情并入实时连接状态，`?project_id=` 返回该项目的有效集合）；`POST /api/projects/{projectId}/mcp/ensure` 对当前项目做一次同样的 reconcile 后返回有效集合及实时状态（前端选中会话时调用，驱动状态栏 MCP 指示器，见 `useMcpStatus`）；连接态的响应额外并入 manager 缓存的 `tools` 列表（原始名/描述，`McpToolSummary`），供 MCP Tools 面板（`McpTabView`，tab kind `mcp`）展示工具清单与失败错误。
- **API** (`oct-agent/api/`): Axum + utoipa OpenAPI generation. Scalar UI at `/scalar`. OpenAPI JSON at `/api/openapi.json`. SSE streaming for chat. Routes:
  - **Providers & Models**: CRUD for providers and models (list, create, update, delete). Models support rich metadata (capabilities, limits, costs, modalities).
  - **Projects**: CRUD (list, create, get, update, delete). Projects have `name` and `working_dir`.
  - **Conversations**: CRUD (list, create, get, update, delete). Conversations belong to a project.
  - **Chat**: Send message (SSE streaming), get messages, manual compaction (`POST /api/conversations/{id}/compact`).
  - **MCP servers**: CRUD at `/api/mcp/servers`（列表/详情并入 `McpManager` 实时连接状态）；`POST /api/projects/{projectId}/mcp/ensure` 先 reconcile 再返回该项目有效集合。
- **AppState**: Holds `SqlitePool`, `Arc<ProviderRegistry>`, `Arc<DashMap<String, RunHandle>>` for tracking active agent sessions, and `Arc<McpManager>` for MCP connections.
- **Frontend** (`packages/app`): Vue 3 + Vite 8 + UnoCSS (OKLCH tokens). Uses `openapi-fetch` to consume the generated OpenAPI schema. `@vueuse/core` for composables. `unplugin-vue-components` for auto-imports. Key directories:
  - `src/composables/`: `useChat.ts`, `useProjects.ts`, `useProviders.ts`, `useMcpServers.ts`, `useContextUsage.ts`（上下文占用指示器）
  - `src/components/chat/`, `diff/`, `file-tree/`, `settings/`, `sidebar/`
  - `src/api/`: `client.ts` (openapi-fetch), `schema.d.ts` (auto-generated)

## Database Schema

6 tables (init `migrations/20260417182650_init_schema` + `migrations/20260922100000_add_mcp_servers`):

- `providers`: id, name, adapter_type, base_url, api_key, doc_url, source (`seeded`/`custom`), timestamps
- `models`: id, provider*id, model_id, name, family, reasoning, tool_call, attachment, structured_output, temperature, knowledge, release_date, open_weights, cost*\_, limit\_\_, modalities\_\*, source, is_enabled, timestamps
- `projects`: id, name, working_dir, additional_dirs (JSON 数组；附加工作目录，绝对路径 only，工具沙箱多根「任一根内即放行」，相对路径仍锚定 working_dir), timestamps
- `conversations`: id, project_id, title, title_source (`default`/`ai`/`user`；手动命名永不被自动标题覆盖，自动标题只在 `default` 时条件写入), timestamps
- `messages`: id, conversation_id, role, parts_json, ordering, provider_id, model_id, input_tokens, output_tokens, reasoning_tokens, created_at
- `mcp_servers`: id, name (UNIQUE), transport (`stdio`；`http` 字段已预留但 API 尚拒绝), command, args_json, env_json, url, headers_json, enabled, tool_timeout_ms, project_id (NULL=全局；非 NULL=仅该项目，FK cascade 随项目删除), timestamps

## Testing

- CI 铁律:测试绝不调用真实模型。所有 LLM 交互都由 mock 驱动(scripted model / 本地 HTTP mock server)。
- `oct-llm-provider` tests use **insta** snapshot testing (YAML). Fixtures are JSON files under `tests/<provider>/fixtures/`. After intentional output changes, run `cargo insta review` to accept new snapshots. Test files: `adapter_openai_compatible.rs`, `providers_anthropic.rs`, `registry.rs`, `anthropic/`, `moonshot/`.
- `oct-llm-provider` **wire-format tests** (`tests/http_wire.rs`): raw-TCP mock server (`tests/http_mock/`) 驱动真实 reqwest + SSE 解码路径,响应体按"恶意"字节边界切块(多字节 UTF-8 中间、SSE 事件中间、data 行中间),并覆盖 401/429/500 状态映射。API key 经环境变量注入(Anthropic)或显式参数注入(OpenAI)。
- `oct-agent` **agent loop integration tests** (`tests/agent_loop.rs`): `tests/support/` 提供 `ScriptedModel`(剧本化 ChatModel,记录收到的每个 ChatRequest)驱动完整生产路径(真实工具 + SQLite + broadcast 事件),覆盖工具调用回路、并行工具、取消三个时机、错误传播。DB 用临时文件 SQLite(`TestEnv`),不要用 `:memory:`(连接池各连接是独立的库)。
- `oct-agent` **API e2e tests** (`tests/api_e2e.rs`): `tests/support` 的 `TestApp` 在随机端口起真实 axum 服务器(独立迁移的临时 SQLite),纯 HTTP 驱动;LLM 是 `tests/support/llm.rs` 的 axum 版 OpenAI 兼容 mock,经 `POST /api/providers` 的 base_url 注入——整条生产链路(路由 → DB 解析 → reqwest → SSE 解码 → agent loop → 真实工具 → 持久化 → SSE 事件流)零生产代码改动地被黑盒覆盖。含 CRUD、工具回路、409 冲突、cancel、错误路径 session 清理。
- 测试禁 sleep 等并发:取消用 `CancellationToken`(同步原语),等事件用 `rx.recv()` + timeout 兜底;SSE 断言以流 EOF 为终止信号(session entry 移除 ⟹ broadcast 关闭 ⟹ body 结束)。
- `oct-agent` 工具层为内联 `#[cfg(test)]` 测试(`src/tools/`)。

## Code Style

- **Comments**: 除非用户明确要求，否则不要删除代码中的注释；但可以更新已有注释以保持准确性
- **Rust edition 2024**: Both crates use `edition = "2024"`
- **Path alias**: Frontend uses `@` → `packages/app/src` (Vite resolve alias)
- **UI primitives**: 前端交互组件（dialog、popover、dropdown、splitter 等）优先使用 [reka-ui](https://reka-ui.com/)，不要手写浮层/焦点管理。文档入口：<https://reka-ui.com/llms.txt>（组件详情在 `/docs/components/<name>.md`）。已在用：`App.vue` 的 Splitter、`SettingsModal.vue` 的 Dialog、`BottomBar.vue` 的 DropdownMenu（含 Sub）、`WorkspaceTabBar.vue` 的 DropdownMenu。
