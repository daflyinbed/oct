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

### LLM wire 日志

- 每个 LLM HTTP 交互的原始 wire 记录（精确请求体 + 每个 framing SSE 事件块 + 流结束/错误），一条一事件的 JSONL，**按对话拆文件**：`{db目录}/llm-wire/{conversation_id}.jsonl`（无会话上下文落 `_orphan.jsonl`；0600；**无总量上限/轮转**；单字段默认 1 MiB 截断标 `truncated`，`OCT_LLM_WIRE_FIELD_MAX=0` 关闭）。
- 开关：设置弹窗 AI 根页（`GET/PUT /api/settings` 的 `llm_wire_log`，持久化在 `settings` 表、运行时即时生效，默认开）；`OCT_LLM_WIRE_LOG=off` 进程级硬关；`OCT_LLM_WIRE_DIR` 改目录。stderr 镜像：`RUST_LOG="oct_llm_provider::wire=trace"`。
- 埋点在 `oct-llm-provider/src/wire.rs`（target `oct_llm_provider::wire`；相关性字段用 task-local tags——host 侧 `wire::with_tags` 包住模型调用，`try_stream!` 生成器由消费方 poll、不在 scope 内，故 `stream()` 入口捕获进 `WireCtx`）。注意 `Layer::register_callsite` 必须返回 `Interest::sometimes()`：默认实现会在开关关闭时缓存 `never`，毒化 callsite 导致再开也不记录。

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

## Docs（改敏感区前先读）

- `docs/explore/`：各主题调研笔记（compaction、MCP、subagent、测试基建、工具调研等）；`docs/backlog.md`：待办。注意 docs/ 整体不入库，均为本地文档；历史架构决策记录（ADR 0001–0004）已删，见 git 历史。

## Architecture

- **Provider abstraction** (`oct-llm-provider`): `Provider` trait creates `ChatModel`. `ProviderRegistry` resolves models by `provider:model_id` spec and supports dynamic custom OpenAI-compatible providers via `resolve_custom_openai_model()`. Adapters (`adapter/openai_compatible.rs`) convert internal types to provider-specific JSON. Concrete providers: Anthropic, MoonshotAI (Kimi).
- **Provider registry** (`oct-llm-provider/provider/registry.rs`): `ProviderRegistry` holds registered providers, resolves chat model specs, and can create ad-hoc OpenAI-compatible chat models for custom providers at runtime.
- **Agent loop** (`oct-agent/agent/`): Streams LLM responses, executes tool calls, feeds results back. Supports cancellation via `tokio_util::sync::CancellationToken`. No iteration limit. Returns a typed `RunOutcome`（终态/最终消息/部分输出标记/聚合 usage——终态结果不靠监听 SSE 拼出；模型流建立阶段同样响应取消）。
- **Run lifecycle** (`oct-agent/runtime/`): `RunService` 统一持有会话占用（`OccupancyGuard`——错误/panic/future drop 均释放，取代手工 sessions.remove 清理）、agent_runs 行持久化（send/resume 各一行；supervisor 内层 spawn 捕获 loop panic、异步终态落库后再释放占用保证 SSE EOF）、模型/工具装配与 `resolve_model/resolve_context_window`（chat 层与子代理共用）；启动 `bootstrap` 把遗留 `running` 行标 `lost`。
- **Subagent runtime** (`oct-agent/subagent/` + `api/subagents.rs`,第一版): `task` 工具（仅 `description`/`prompt` 两参数，角色固定 worker、depth≤1 服务端校验）把自包含子任务委派给独立子会话——复用同一个 `run_agent_loop` 与 RunService,不递归 LLM 调用器、不走 REST。子会话 `conversations.kind='subagent'`（不进侧栏、send/resume/compact 409 `managed_conversation`）；注册幂等：同 `(parent_run_id, parent_tool_call_id)` 恒一个子 run，子会话+委派消息+run 行单事务创建。子工具集=父集合−task（`read/write/edit_file` 按共享 `WorkspaceFileLocks`（RunService 按项目共享）+每 run 独立 `AgentReadState` 重建，listdir/grep/glob/execute/MCP 复用父 Arc 实例——schema 与派发路由精确等价）；子取消 token 是父的 `child_token()`。settlement=epoch 守卫 `finish_run`+父 hub `subagent_updated` 小事件+done watch,内存只存活动对象（子 hub 为 Weak,不阻 SSE EOF）；信封（status/output/truncated/usage-null=unknown）由 DB 行单一事实源构建，live/回填/恢复三路一致。崩溃恢复:resume 立即返回 SSE，orchestrator 识别受管 task——终态直接回填（子已完成父未接收不重跑模型）、活动接回等待、`lost` 同 run 行续跑（epoch+1,旧实例迟到写入被守卫拒绝）；「工具结果齐+末条 tool+run lost」纳入恢复资格；存在未结算委派时 send/compact 409 `pending_recovery`。删除父会话先取消并等待子代理结算。`insert_message` 带 SQLITE_BUSY(5/517) 重试（父子 loop 并发写库时 deferred 事务读→写升级冲突的标准化应对）。
- **Background bash runtime** (`oct-agent/runtime/background.rs` + `tools/task_output.rs`/`task_stop.rs` + `api/background.rs`,第一版): `execute_command` 的 `run_in_background` 把长命令交给独立任务运行时——**envelope 即答**（tool call 立即闭环,与 dangling 修复/恢复资格零交互）、子进程 stdout/stderr **fd 直写**工作区外的输出文件（`{db目录}/exec/{conv}/{task_id}.log`,沙箱 read 够不到,模型经 `task_output` 读:默认尾部/offset 分页/block 有界等待）、内存注册表 + `background_tasks` 表单一事实源（终态一次性守卫落库;bootstrap 把 running 标 lost）。终态后 per-conversation 投递循环等会话空闲（occupy 串行合批）→ 单事务插 `<task-notification>` user 消息（尾部 4KiB 内联）+ `notified` 置位 → 以会话上次 provider/model 起 wake run（不触发 title）;send/resume 入口 catch-up 补投未通知终态。取消语义解耦：进程组+PDEATHSIG 沿用（随后端死）但**不随 run 死**——只有 task_stop/HTTP cancel/会话删除两阶段杀（SIGTERM→1.5s→SIGKILL）;watchdog 1GiB 输出上限 + 24h 运行上限。子代理 gate：`derive_child_toolset` 重建 execute（schema 同、dispatch 关）并丢弃 task_output/task_stop。
- **Agent tools** (`oct-agent/tools/`): `read`, `write`, `edit_file`, `listdir`, `grep`, `glob`, `execute`（含 `run_in_background` 后台分支）, `task_output`/`task_stop`（后台任务读取/停止,仅 root toolset）（文件工具的状态拆分为共享 `WorkspaceFileLocks` + 每 run 的 `AgentReadState`——跨 agent 文件锁串行、阅读记录互不授权；grep/glob 共用 `search_common`，多根工作区沙箱解析在 `workspace`）
- **Microcompact** (`oct-agent/agent/micro.rs` + loop 集成 + `db/messages.rs::apply_cow_swaps`): 摘要压缩前的廉价第一级——不调模型，turn-top（在 compact 检查之前）把旧的成功态工具结果 copy-on-write 替换为占位符 `[Old tool result content cleared]`（content 换占位符、images 清空、call_id/is_error 不动；副本行 `supersedes_message_id` 回指原文恒一跳，details_json `kind:"microcompact_cleared"`）。双入口：水位 `est ≥ 窗口×70%`（compact 是 85%；窗口未知禁用）与空闲（run 启动距最后一条消息 ≥ 60min，时点在 API 层 repair/插入用户消息**之前**捕获经 `AgentContext.idle` 传入，首个 turn-top 一次性触发、无视窗口）。选择器永不清错误结果（摘要 prompt 要逐字保留）、不动最后一轮（`last_round_start`）与其前最近 5 个 tool 行、跳过已是占位符的行（幂等由构造保证）、预计节省 < 1024 token 不动手；DB 层幂等由「解析槽位时排除已是副本的行」保证。anchor 内存修正与 `load_llm_view` 重算规则一致（清在 anchor 之前的槽位减 savings；loop 的运行态视图/锚点/序号统一由 `context::ContextState` 持有与变换）。SSE 事件 `microcompacted`（带 `replacements: [{call_id, copy_message_id}]` 映射，前端按 call_id 原地折叠历史工具卡片，重放幂等）；`GET /api/conversations/{id}/messages/{copy_id}/original` 经副本 membership 作用域校验后沿 supersedes 一跳取原文。前端：工具卡片折叠为「已清除」徽标 +「查看原文」懒加载。
- **Context state**（`oct-agent/src/context.rs`）: DB 与 agent 共享的中性层——纯 token 估算（`estimate_message/estimate_context/estimate_view`）、marker/COW 副本识别（`details_kind`/`COMPACTION_MARKER_KIND`）、`LlmView`/`UsageAnchor` 快照类型，以及 loop 的运行态 `ContextState`（system prompt + 带 ordering 的 `ContextEntry` + anchor；`append`/`apply_compaction`/`apply_clears` 集中变换，与重新 `load_llm_view` 的有效消息/ordering/anchor 保持一致——消除了 db→agent 的反向依赖与平行的 messages/orderings 双向量）。
- **Context compaction** (`oct-agent/agent/compact.rs` + `db/messages.rs::load_llm_view` + loop 集成): 对话过长时把切割点之前的历史摘要成一条标记行。触发：turn 顶部（每次 LLM 请求前，覆盖 run 首轮与工具轮之间）按「provider 真实 usage 锚点（上一笔请求 input+output）+ 本地增量估算（ASCII÷4 + 非 ASCII×1；无锚点时含 system+工具 schema）」对比窗口 ×0.85，或手动 `/compact`。窗口解析链 `ModelInfo.limits.max_input_tokens` → DB `models.limit_context` → 未知则禁用自动。压缩 = 同模型单轮无工具 `generate`（结构化分段 prompt，输入序列化截断：reasoning 丢弃、tool result 截 1500 字符、图片按 1200/张计、总量超限中部丢弃）+ 落一条 role=user 标记行（parts 带摘要框架文本，details_json `kind:"compaction"` 元数据）——手动（`api/chat.rs::run_manual_compact`）与自动（loop turn-top）共用同一个压缩事务 `compact::run_compaction`（类型化 `CompactRequest`/`CompactOutcome`：切点选择、摘要、marker/details 构造、`insert_message_at_ordering` 两阶段腾位**插入在切点槽位**——region 与 tail 之间，追加到末尾会让 tail 落到 marker 之前、下个 run 从视图消失；触发策略/错误映射/事件归各自入口）；摘要失败只禁用自动压缩，水位 micro 开关独立不受影响；**老行永不删除**，`load_llm_view` 只返回「最新标记行之后」的视图并过滤孤儿 ToolResult（repair 也只修标记之后的悬空调用）。anchor 选取要求 assistant 的 `appended_at_revision` 晚于 marker（tail 里早于 marker 的旧 usage 含被摘要掉的前缀，会导致重载后估算虚高；无压缩后新 usage 时恢复为 anchor=None）。尾部 verbatim 保留（窗口/10，封顶 32k，绝不拆 tool_call/tool_result 对）；防死循环（再触发需 est ≥ 压缩后估算 + tail 预算；摘要失败本 run 禁用自动）。SSE 事件 `compacting`/`compacted`（带全量 summary）；`usage` 事件带 `context_window_tokens`；手动端点 `POST /api/conversations/{id}/compact`（session 活动中 409）。前端：`CompactionDivider` 分隔条（stored 标记行 + live 事件两路归一）、`useContextUsage` + 输入框内 `ContextRing` 上下文占用圆环（hover 弹容量详情）、输入框 `/compact` 命令拦截。
- **MCP support** (`oct-mcp` + `oct-agent/src/mcp/`（`manager`/`connect`/`credential`/`catalog`/`tool` 按职责分文件）+ `oct-agent/api/mcp.rs` + `oct-agent/db/mcp.rs` + `oct-agent/src/oauth.rs` + `oct-agent/db/mcp_tokens.rs`): MCP 客户端 Phase 2（tools + prompts + resources、stdio + Streamable HTTP、OAuth）。`oct-mcp` 隔离 rmcp（features：`client` + `transport-child-process` + `transport-streamable-http-client-reqwest`），公开窄接口 `McpConnection`（list/call tools、list/render prompts、list/read resources、`tool_list_changed` 通知通道）、`connect_stdio`/`connect_http`（custom headers 透传 Authorization）、`qualified_tool_name`（`mcp__{server}__{tool}`）/`ToolNameMap`；自定义 rmcp ClientHandler（`NotifyHandler`）接 `tools/list_changed` → `watch<u64>` 计数通道。`McpManager` 进 AppState，每个 chat turn 前按当前项目对账 enabled `mcp_servers` 行（作用域两级合并；增删改/指纹变更重连、失败记 Failed；连接 key 是 server id 跨项目复用。并发模型：决策在短 `decide_lock` 内无网络等待地完成，连接/移除各只持有该 server 的 gate——同 server 并发 reconcile 共享一次握手、不同 server（不同项目）互不排队；OAuth `invalidate` 不等在途握手而是推进失效 epoch，在途连接提交前校验 epoch，迟到结果不回写），连接时同时拉取 tools/prompts/resources 目录，`tools/list_changed` 通知 500ms 去抖后重 list 刷新（`ManagedServer.generation` 防旧监听任务写新 entry）；工具以 `McpTool` 拼进 run toolset，超时默认 60s（行级 `tool_timeout_ms` 覆盖），取消经 `ToolContext.cancel_token()` select 中断在飞调用（不 shutdown 共享连接）；stdio 子进程与 `execute` 工具同设 Linux PDEATHSIG 防孤儿。工具结果多模态：`McpCallResult.images`（base64）→ `ToolOutput.images` → `ToolResult.images`，anthropic 侧 tool_result 变块数组（text + image base64 source），openai 兼容侧在连续 tool 消息后追加一条 user 消息带 data URL（无图时两侧行为/快照不变）。OAuth：显式发起（`POST /api/mcp/servers/{id}/oauth/start` 做 RFC 8414 发现 + RFC 7591 DCR + PKCE S256 + resource 参数）→ `GET /api/mcp/oauth/callback`（state 单次）换 token 存 `mcp_server_tokens` 表 → 连接时 Bearer 合入 headers（token 优先于 headers_json）、过期自动刷新、失败提示重登；token/secret 不进日志与 OpenAPI。prompts 经斜杠命令消费（`useSlashCommands`：输入 `/` 弹菜单，内置命令（如 `/compact`，ChatPanel 就地注册）+ 各 server prompts 分组；带参 prompt 弹 `PromptArgsDialog`，渲染结果填入输入框不自动发送；后端 `POST /api/mcp/servers/{id}/prompts/{name}`）；resources 只读侧在 MCP 面板查看（`POST /api/mcp/servers/{id}/resources/read`）。REST CRUD 在 `/api/mcp/servers`（列表/详情并入实时连接状态与 tools/prompts/resources 目录）；`POST /api/projects/{projectId}/mcp/ensure` reconcile 后返回有效集合（驱动状态栏指示器与 `McpTabView` 面板）。
- **API** (`oct-agent/api/`): Axum + utoipa OpenAPI generation. Scalar UI at `/scalar`. OpenAPI JSON at `/api/openapi.json`. SSE streaming for chat. Routes:
  - **Providers & Models**: CRUD for providers and models (list, create, update, delete). Models support rich metadata (capabilities, limits, costs, modalities).
  - **Projects**: CRUD (list, create, get, update, delete). Projects have `name` and `working_dir`.
  - **Files**: `GET /api/projects/{projectId}/files`（项目文件树）、`GET /api/projects/{projectId}/files/content`（读单文件，文件预览标签页用）
  - **Git**: `GET /api/projects/{projectId}/git/status|diff|history|stashes` + `POST /api/projects/{projectId}/git/actions`（stage/unstage/commit/push 等全量操作，驱动前端 Changes 面板）
  - **Conversations**: CRUD (list, create, get, update, delete). Conversations belong to a project.
  - **Chat**: Send message (SSE streaming), get messages, manual compaction (`POST /api/conversations/{id}/compact`). Resume (`POST /api/conversations/{id}/resume`) doubles as the subagent crash-recovery entry（有受管委派时 orchestrator 先重建任务树再续父 loop）。
  - **Settings**: `GET/PUT /api/settings`（应用设置;v1 键 `llm_wire_log`——PUT 落库 + 进程内开关即时生效）。
  - **Subagents**: `GET /api/conversations/{id}/subagents`（根会话任务快照,running 优先）; `GET /api/subagents/{agentId}/messages|events`（子历史/活动 run 的 SSE,含 run_meta 注入）; `POST /api/subagents/{agentId}/cancel`（单独取消;lost run 持久化为 cancelled）。
  - **Background tasks**（前端面）: `GET /api/conversations/{id}/background-tasks`（任务列表,running 优先）; `GET /api/background-tasks/{id}/output?offset&limit`（有界输出切片）; `POST /api/background-tasks/{id}/cancel`。
  - **MCP servers**: CRUD at `/api/mcp/servers`（列表/详情并入 `McpManager` 实时连接状态与 tools/prompts/resources 目录）；`POST /api/projects/{projectId}/mcp/ensure` 先 reconcile 再返回该项目有效集合；`POST /api/mcp/servers/{id}/prompts/{name}` 渲染 prompt（带参）；`POST /api/mcp/servers/{id}/resources/read` 读 resource；`POST /api/mcp/servers/{id}/oauth/start` + `GET /api/mcp/oauth/callback` + `DELETE /api/mcp/servers/{id}/oauth` 构成 OAuth 授权码 + PKCE 流。
- **AppState**: Holds `SqlitePool`, `Arc<ProviderRegistry>`, `Arc<DashMap<String, RunHandle>>` for tracking active agent sessions, `Arc<McpManager>` for MCP connections, `Arc<OAuthFlowStore>` for in-flight MCP OAuth authorization flows (memory-only), `Arc<RunService>`（`state.runs`——占用/装配/supervisor 与子代理/后台任务管理）, and `WireLogControl`（`state.wire_log`——LLM wire 日志开关的进程级句柄,settings API 即时翻转）。
- **Frontend** (`packages/app`): Vue 3 + Vite 8 + UnoCSS (OKLCH tokens). Uses `openapi-fetch` to consume the generated OpenAPI schema. `@vueuse/core` for composables. `unplugin-vue-components` for auto-imports. `shiki`（`utils/highlight.ts`）做代码高亮. Key directories:
  - `src/composables/`: `useChat.ts`（`createChatSession()` 工厂——主面板单例 `useChat()` 之外,子代理面板持有实例化会话状态:`attachSubagent`/`dispose`/`contextUsage:false`）, `useSubagents.ts`（根会话任务 store:快照+`subagent_updated` upsert/epoch 去重,TaskCard 与底部指示器共用）, `useProjects.ts`, `useProviders.ts`, `useMcpServers.ts` + `useMcpServerForm.ts`, `useMcpStatus.ts`, `useSlashCommands.ts`（斜杠命令菜单）, `useBackgroundTasks.ts`（后台 bash 任务快照 store:列表轮询(存在非终态且页面可见时 2s)+cancel upsert+唤醒 run 宽限窗探测）, `useContextUsage.ts`（上下文占用指示器）, `useGit.ts`（Changes 面板：status/diff/history/stashes/actions）, `useFileContent.ts`（文件预览）, `useTabs.ts`（工作区标签页）
  - `src/components/`: `chat/`（含 BashTaskCard 后台任务卡/TaskNotificationDivider 通知分隔条/输出查看 Dialog）, `subagent/`（TaskCard/子代理面板）, `changes/`（Git Changes）, `diff/`, `file-tree/`, `views/`（标签页视图）, `layout/`, `ui/`, `settings/`, `sidebar/`
  - `src/api/`: `client.ts` (openapi-fetch), `schema.d.ts` (auto-generated)

## Database Schema

11 tables（migrations 按时间序累加：`init_schema` → `add_details_json_to_messages` → `add_conversation_title_source` → `add_project_additional_dirs` → `add_mcp_servers` → `add_project_scope_to_mcp_servers` → `add_mcp_server_tokens` → `split_message_membership` → `add_agent_runs` → `add_subagent_conversations` → `add_background_tasks` → `add_tool_permissions` → `add_settings`）:

- `providers`: id, name, adapter_type, base_url, api_key, doc_url, source (`seeded`/`custom`), timestamps
- `models`: id, provider*id, model_id, name, family, reasoning, tool_call, attachment, structured_output, temperature, knowledge, release_date, open_weights, cost*\_, limit\_\_, modalities\_\*, source, is_enabled, timestamps
- `projects`: id, name, working_dir, additional_dirs (JSON 数组；附加工作目录，绝对路径 only，工具沙箱多根「任一根内即放行」，相对路径仍锚定 working_dir), timestamps
- `conversations`: id, project_id, title, title_source (`default`/`ai`/`user`；手动命名永不被自动标题覆盖，自动标题只在 `default` 时条件写入), kind (`chat`/`subagent`), parent_conversation_id, root_conversation_id, agent_type (`worker`;子会话不进侧栏、不走普通 send/resume/compact), context_revision (会话 revision 计数器：每次 membership 变更在同事务内 +1 并盖章到行), timestamps
- `agent_runs`: 根/子共用的 run 行——id, conversation_id, kind (`root`/`subagent`), parent_run_id, root_run_id, parent_tool_call_id（(parent_run_id, parent_tool_call_id) 注册幂等键）, status (`running`/`completed`/`failed`/`cancelled`/`lost`——lost=可恢复中断), execution_epoch（crash 续跑递增,迟到实例写入被守卫拒绝）, provider_id/model_id/context_window（有效配置快照,不含密钥）, description, error, final_message_id, input_tokens/output_tokens, started_at, finished_at
- `messages`: **纯内容行，insert-only**（唯一后续写 = usage 回填）——id, role, parts_json, details_json (UI-only), provider_id, model_id, input_tokens, output_tokens, reasoning_tokens, supersedes_message_id (COW 副本回指原文，恒一跳), created_at。跨 fork 共享
- `conversation_messages`: 会话归属时间线——conversation_id (FK cascade), message_id (FK cascade), ordering (UNIQUE per conversation), appended_at_revision (插入时盖章、永不变；回滚/fork 切点与 anchor 资格判定的时点), swapped_at_revision (COW 换指针时盖章；NULL=未替换)。fork=复制行共享内容行，回滚=截断，microcompact COW=拨 message_id 指针（`db::messages::apply_cow_swaps`）
- `mcp_servers`: id, name (UNIQUE), transport (`stdio`/`http`), command, args_json, env_json, url, headers_json (http 侧静态 headers/bearer), enabled, tool_timeout_ms, project_id (NULL=全局；非 NULL=仅该项目，FK cascade 随项目删除), timestamps
- `mcp_server_tokens`: server_id (PK=UNIQUE, FK cascade → mcp_servers), client_id, client_secret, access_token, refresh_token, token_type, scope, expires_at, obtained_at, timestamps（OAuth 凭据，永不经 API 返回）
- `background_tasks`: id（调用方生成,`bg_{uuid}`——注册表键=输出文件名=行 id）, conversation_id (FK cascade), parent_run_id (FK cascade → agent_runs), parent_call_id, command, working_dir, status (`running`/`completed`/`failed`/`cancelled`/`lost`), exit_code, output_path, output_bytes, error, notified（0/1,与通知消息插入同事务置位）, created_at, finished_at（内存只存活动任务）
- `settings`: key (PK), value, updated_at——进程级开关的持久侧（KV 存储,已知键归 API 层的类型化 DTO 管;v1 键 `llm_wire_log`='on'/'off'）

## Testing

- CI 铁律:测试绝不调用真实模型。所有 LLM 交互都由 mock 驱动(scripted model / 本地 HTTP mock server)。
- `oct-llm-provider` tests use **insta** snapshot testing (YAML). Fixtures are JSON files under `tests/<provider>/fixtures/`. After intentional output changes, run `cargo insta review` to accept new snapshots. Test files: `adapter_openai_compatible.rs`, `providers_anthropic.rs`, `registry.rs`, `anthropic/`, `moonshot/`.
- `oct-llm-provider` **wire-format tests** (`tests/http_wire.rs`): raw-TCP mock server (`tests/http_mock/`) 驱动真实 reqwest + SSE 解码路径,响应体按"恶意"字节边界切块(多字节 UTF-8 中间、SSE 事件中间、data 行中间),并覆盖 401/429/500 状态映射。API key 经环境变量注入(Anthropic)或显式参数注入(OpenAI)。
- `oct-agent` **agent loop integration tests** (`tests/agent_loop.rs`): `tests/support/` 提供 `ScriptedModel`(剧本化 ChatModel,记录收到的每个 ChatRequest)驱动完整生产路径(真实工具 + SQLite + broadcast 事件),覆盖工具调用回路、并行工具、取消三个时机、错误传播。DB 用临时文件 SQLite(`TestEnv`),不要用 `:memory:`(连接池各连接是独立的库)。
- `oct-agent` **API e2e tests** (`tests/api_e2e.rs`): `tests/support` 的 `TestApp` 在随机端口起真实 axum 服务器(独立迁移的临时 SQLite),纯 HTTP 驱动;LLM 是 `tests/support/llm.rs` 的 axum 版 OpenAI 兼容 mock,经 `POST /api/providers` 的 base_url 注入——整条生产链路(路由 → DB 解析 → reqwest → SSE 解码 → agent loop → 真实工具 → 持久化 → SSE 事件流)零生产代码改动地被黑盒覆盖。含 CRUD、工具回路、409 冲突、cancel、错误路径 session 清理。llm mock 支持按 marker 的按 agent 剧本路由(`push_marker_turns`:请求的 user 角色内容含 marker 走该队列,命中 marker 的空队列不回退全局——父与多个子共享一个 mock 各自独立剧本;并行 tool call 需 `tool_call_chunk_at` 传不同 index)。
- `oct-agent` **subagent 集成测试** (`tests/subagent_runtime.rs`): 全链路驱动 task 委派——并行重叠证明(gate 停 A、B 先完成)、父取消级联、单子取消/单子失败不影响兄弟、跨 agent 阅读态隔离(未读报错+A 写后 B 旧读 stale,mtime 粒度竞态用显式 set_times 钉死)、MCP 继承(schema+派发等价、共享连接)、重启恢复(WAL checkpoint+db 文件复制+`TestApp::new_with_dir` 模拟 crash;epoch=2、结果只落一次、pending_recovery 409)、终态回填不重跑子模型、lost+tool 尾恢复资格、managed 409。等事件用 SSE 事件/流 EOF(结算信号),无 sleep。
- `oct-agent` **background task 集成测试** (`tests/background_tasks.rs`): 全链路驱动 run_in_background——信封即答、运行中输出文件可读（fd 直写）、结算一次、通知唤醒（断言 LlmMock 收到的请求含 `<task-notification>`/exit_code/尾部）、run 取消不杀后台、HTTP cancel 结算、watchdog 击杀（上限缩 256B）、崩溃模拟（WAL checkpoint+db 复制+`new_with_dir`）→ lost+send catch-up 前置通知、会话删除级联、工具级直调（task_output 阻塞/分页、task_stop、子代理 gate）。唤醒 run 无测试侧订阅,用「轮询 LlmMock 请求日志+30s 截止」的有界等待。
- 测试禁 sleep 等并发:取消用 `CancellationToken`(同步原语),等事件用 `rx.recv()` + timeout 兜底;SSE 断言以流 EOF 为终止信号(session entry 移除 ⟹ broadcast 关闭 ⟹ body 结束)。
- `oct-agent` 工具层为内联 `#[cfg(test)]` 测试(`src/tools/`)。

## Code Style

- **Comments**: 除非用户明确要求，否则不要删除代码中的注释；但可以更新已有注释以保持准确性
- **Rust edition 2024**: Both crates use `edition = "2024"`
- **Path alias**: Frontend uses `@` → `packages/app/src` (Vite resolve alias)
- **UI primitives**: 前端交互组件（dialog、popover、dropdown、splitter 等）优先使用 [reka-ui](https://reka-ui.com/)，不要手写浮层/焦点管理。文档入口：<https://reka-ui.com/llms.txt>（组件详情在 `/docs/components/<name>.md`）。已在用：`App.vue` 的 Splitter、`SettingsModal.vue` 的 Dialog、`BottomBar.vue` 的 DropdownMenu（含 Sub）、`WorkspaceTabBar.vue` 的 DropdownMenu。
