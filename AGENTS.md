# AGENTS.md

## Monorepo Layout

Two separate package managers coexist:

- **Rust workspace** (`Cargo.toml`): `crates/oct-llm-provider` (library) → `crates/oct-mcp` (MCP client quarantine crate: wraps rmcp behind a narrow `McpConnection` trait; stdio + Streamable HTTP transports, reqwest 全仓统一 0.13) → `crates/oct-agent` (library + `oct-agent` binary; integration tests import `oct_agent::…`)
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
- **Microcompact** (`oct-agent/agent/micro.rs` + loop 集成 + `db/messages.rs::apply_cow_swaps`): 摘要压缩前的廉价第一级——不调模型，turn-top（在 compact 检查之前）把旧的成功态工具结果 copy-on-write 替换为占位符 `[Old tool result content cleared]`（content 换占位符、images 清空、call_id/is_error 不动；副本行 `supersedes_message_id` 回指原文恒一跳，details_json `kind:"microcompact_cleared"`）。双入口：水位 `est ≥ 窗口×70%`（compact 是 85%；窗口未知禁用）与空闲（run 启动距最后一条消息 ≥ 60min，时点在 API 层 repair/插入用户消息**之前**捕获经 `AgentContext.idle` 传入，首个 turn-top 一次性触发、无视窗口）。选择器永不清错误结果（摘要 prompt 要逐字保留）、不动最后一轮（`last_round_start`）与其前最近 5 个 tool 行、跳过已是占位符的行（幂等由构造保证）、预计节省 < 1024 token 不动手；DB 层幂等由「解析槽位时排除已是副本的行」保证。anchor 内存修正与 `load_llm_view` 重算规则一致（清在 anchor 之前的槽位减 savings）。SSE 事件 `microcompacted`（带 `replacements: [{call_id, copy_message_id}]` 映射，前端按 call_id 原地折叠历史工具卡片，重放幂等）；`GET /api/conversations/{id}/messages/{copy_id}/original` 经副本 membership 作用域校验后沿 supersedes 一跳取原文。前端：工具卡片折叠为「已清除」徽标 +「查看原文」懒加载。
- **Context compaction** (`oct-agent/agent/compact.rs` + `db/messages.rs::load_llm_view` + loop 集成): 对话过长时把切割点之前的历史摘要成一条标记行。触发：turn 顶部（每次 LLM 请求前，覆盖 run 首轮与工具轮之间）按「provider 真实 usage 锚点（上一笔请求 input+output）+ 本地增量估算（ASCII÷4 + 非 ASCII×1；无锚点时含 system+工具 schema）」对比窗口 ×0.85，或手动 `/compact`。窗口解析链 `ModelInfo.limits.max_input_tokens` → DB `models.limit_context` → 未知则禁用自动。压缩 = 同模型单轮无工具 `generate`（结构化分段 prompt，输入序列化截断：reasoning 丢弃、tool result 截 1500 字符、图片按 1200/张计、总量超限中部丢弃）+ 落一条 role=user 标记行（parts 带摘要框架文本，details_json `kind:"compaction"` 元数据）——marker 经 `insert_message_at_ordering` 两阶段腾位**插入在切点槽位**（region 与 tail 之间；追加到末尾会让 tail 落到 marker 之前、下个 run 从视图消失），loop 用与 `full_messages` 平行的 `message_orderings` 向量维护槽位；**老行永不删除**，`load_llm_view` 只返回「最新标记行之后」的视图并过滤孤儿 ToolResult（repair 也只修标记之后的悬空调用）。anchor 选取要求 assistant 的 `appended_at_revision` 晚于 marker（tail 里早于 marker 的旧 usage 含被摘要掉的前缀，会导致重载后估算虚高；无压缩后新 usage 时恢复为 anchor=None）。尾部 verbatim 保留（窗口/10，封顶 32k，绝不拆 tool_call/tool_result 对）；防死循环（再触发需 est ≥ 压缩后估算 + tail 预算；摘要失败本 run 禁用自动）。SSE 事件 `compacting`/`compacted`（带全量 summary）；`usage` 事件带 `context_window_tokens`；手动端点 `POST /api/conversations/{id}/compact`（session 活动中 409）。前端：`CompactionDivider` 分隔条（stored 标记行 + live 事件两路归一）、`useContextUsage` + 输入框内 `ContextRing` 上下文占用圆环（hover 弹容量详情）、输入框 `/compact` 命令拦截。
- **MCP support** (`oct-mcp` + `oct-agent/src/mcp.rs` + `oct-agent/api/mcp.rs` + `oct-agent/db/mcp.rs` + `oct-agent/src/oauth.rs` + `oct-agent/db/mcp_tokens.rs`): MCP 客户端 Phase 2（tools + prompts + resources、stdio + Streamable HTTP、OAuth）。`oct-mcp` 隔离 rmcp（features：`client` + `transport-child-process` + `transport-streamable-http-client-reqwest`），公开窄接口 `McpConnection`（list/call tools、list/render prompts、list/read resources、`tool_list_changed` 通知通道）、`connect_stdio`/`connect_http`（custom headers 透传 Authorization）、`qualified_tool_name`（`mcp__{server}__{tool}`）/`ToolNameMap`；自定义 rmcp ClientHandler（`NotifyHandler`）接 `tools/list_changed` → `watch<u64>` 计数通道。`McpManager` 进 AppState，每个 chat turn 前按当前项目对账 enabled `mcp_servers` 行（作用域两级合并；增删改/指纹变更重连、失败记 Failed；连接 key 是 server id 跨项目复用），连接时同时拉取 tools/prompts/resources 目录，`tools/list_changed` 通知 500ms 去抖后重 list 刷新（`ManagedServer.generation` 防旧监听任务写新 entry）；工具以 `McpTool` 拼进 run toolset，超时默认 60s（行级 `tool_timeout_ms` 覆盖），取消经 `ToolContext.cancel_token()` select 中断在飞调用（不 shutdown 共享连接）；stdio 子进程与 `execute` 工具同设 Linux PDEATHSIG 防孤儿。工具结果多模态：`McpCallResult.images`（base64）→ `ToolOutput.images` → `ToolResult.images`，anthropic 侧 tool_result 变块数组（text + image base64 source），openai 兼容侧在连续 tool 消息后追加一条 user 消息带 data URL（无图时两侧行为/快照不变）。OAuth：显式发起（`POST /api/mcp/servers/{id}/oauth/start` 做 RFC 8414 发现 + RFC 7591 DCR + PKCE S256 + resource 参数）→ `GET /api/mcp/oauth/callback`（state 单次）换 token 存 `mcp_server_tokens` 表 → 连接时 Bearer 合入 headers（token 优先于 headers_json）、过期自动刷新、失败提示重登；token/secret 不进日志与 OpenAPI。prompts 经斜杠命令消费（`useSlashCommands`：输入 `/` 弹菜单，内置命令（如 `/compact`，ChatPanel 就地注册）+ 各 server prompts 分组；带参 prompt 弹 `PromptArgsDialog`，渲染结果填入输入框不自动发送；后端 `POST /api/mcp/servers/{id}/prompts/{name}`）；resources 只读侧在 MCP 面板查看（`POST /api/mcp/servers/{id}/resources/read`）。REST CRUD 在 `/api/mcp/servers`（列表/详情并入实时连接状态与 tools/prompts/resources 目录）；`POST /api/projects/{projectId}/mcp/ensure` reconcile 后返回有效集合（驱动状态栏指示器与 `McpTabView` 面板）。
- **API** (`oct-agent/api/`): Axum + utoipa OpenAPI generation. Scalar UI at `/scalar`. OpenAPI JSON at `/api/openapi.json`. SSE streaming for chat. Routes:
  - **Providers & Models**: CRUD for providers and models (list, create, update, delete). Models support rich metadata (capabilities, limits, costs, modalities).
  - **Projects**: CRUD (list, create, get, update, delete). Projects have `name` and `working_dir`.
  - **Conversations**: CRUD (list, create, get, update, delete). Conversations belong to a project.
  - **Chat**: Send message (SSE streaming), get messages, manual compaction (`POST /api/conversations/{id}/compact`).
  - **MCP servers**: CRUD at `/api/mcp/servers`（列表/详情并入 `McpManager` 实时连接状态与 tools/prompts/resources 目录）；`POST /api/projects/{projectId}/mcp/ensure` 先 reconcile 再返回该项目有效集合；`POST /api/mcp/servers/{id}/prompts/{name}` 渲染 prompt（带参）；`POST /api/mcp/servers/{id}/resources/read` 读 resource；`POST /api/mcp/servers/{id}/oauth/start` + `GET /api/mcp/oauth/callback` + `DELETE /api/mcp/servers/{id}/oauth` 构成 OAuth 授权码 + PKCE 流。
- **AppState**: Holds `SqlitePool`, `Arc<ProviderRegistry>`, `Arc<DashMap<String, RunHandle>>` for tracking active agent sessions, `Arc<McpManager>` for MCP connections, and `Arc<OAuthFlowStore>` for in-flight MCP OAuth authorization flows (memory-only).
- **Frontend** (`packages/app`): Vue 3 + Vite 8 + UnoCSS (OKLCH tokens). Uses `openapi-fetch` to consume the generated OpenAPI schema. `@vueuse/core` for composables. `unplugin-vue-components` for auto-imports. Key directories:
  - `src/composables/`: `useChat.ts`, `useProjects.ts`, `useProviders.ts`, `useMcpServers.ts`, `useMcpStatus.ts`, `useSlashCommands.ts`（斜杠命令菜单）, `useContextUsage.ts`（上下文占用指示器）
  - `src/components/chat/`, `diff/`, `file-tree/`, `settings/`, `sidebar/`
  - `src/api/`: `client.ts` (openapi-fetch), `schema.d.ts` (auto-generated)

## Database Schema

8 tables (init `migrations/20260417182650_init_schema` + `migrations/20260922100000_add_mcp_servers` + `migrations/20260923110000_add_mcp_server_tokens` + `migrations/20260923140000_split_message_membership`):

- `providers`: id, name, adapter_type, base_url, api_key, doc_url, source (`seeded`/`custom`), timestamps
- `models`: id, provider*id, model_id, name, family, reasoning, tool_call, attachment, structured_output, temperature, knowledge, release_date, open_weights, cost*\_, limit\_\_, modalities\_\*, source, is_enabled, timestamps
- `projects`: id, name, working_dir, additional_dirs (JSON 数组；附加工作目录，绝对路径 only，工具沙箱多根「任一根内即放行」，相对路径仍锚定 working_dir), timestamps
- `conversations`: id, project_id, title, title_source (`default`/`ai`/`user`；手动命名永不被自动标题覆盖，自动标题只在 `default` 时条件写入), context_revision (会话 revision 计数器：每次 membership 变更在同事务内 +1 并盖章到行), timestamps
- `messages`: **纯内容行，insert-only**（唯一后续写 = usage 回填）——id, role, parts_json, details_json (UI-only), provider_id, model_id, input_tokens, output_tokens, reasoning_tokens, supersedes_message_id (COW 副本回指原文，恒一跳), created_at。跨 fork 共享
- `conversation_messages`: 会话归属时间线——conversation_id (FK cascade), message_id (FK cascade), ordering (UNIQUE per conversation), appended_at_revision (插入时盖章、永不变；回滚/fork 切点与 anchor 资格判定的时点), swapped_at_revision (COW 换指针时盖章；NULL=未替换)。fork=复制行共享内容行，回滚=截断，microcompact COW=拨 message_id 指针（`db::messages::apply_cow_swaps`）
- `mcp_servers`: id, name (UNIQUE), transport (`stdio`/`http`), command, args_json, env_json, url, headers_json (http 侧静态 headers/bearer), enabled, tool_timeout_ms, project_id (NULL=全局；非 NULL=仅该项目，FK cascade 随项目删除), timestamps
- `mcp_server_tokens`: server_id (PK=UNIQUE, FK cascade → mcp_servers), client_id, client_secret, access_token, refresh_token, token_type, scope, expires_at, obtained_at, timestamps（OAuth 凭据，永不经 API 返回）

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
