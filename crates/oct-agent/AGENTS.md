# oct-agent AGENTS.md

后端服务 crate。用户侧概览见 `README.md`；数据库/构建/迁移的全局约定见仓库根 `AGENTS.md`。

## Architecture

- **Agent loop** (`agent/`): 流式 LLM 响应；工具调用跑在自有 `JoinSet`（drop/cancel 中止未完成调用），兄弟工具继续执行、完成即落库（避免 SQLite 事务死锁）；取消走 `CancellationToken`，无迭代上限；返回类型化 `RunOutcome`——终态/最终消息/部分输出不靠监听 SSE 拼出。
- **Run lifecycle** (`runtime/`): `RunService` 统一持有会话占用（`OccupancyGuard`——错误/panic/future drop 均释放）、agent_runs 持久化（supervisor 内层 spawn 捕获 loop panic、终态落库后再释放占用，保证 SSE EOF）、模型/工具装配；启动 `bootstrap` 把遗留 `running` 行标 `lost`（=可恢复中断）。
- **Subagent** (`subagent/`): `task` 工具委派独立子会话（`kind='subagent'`，普通 send/resume/compact 409 `managed_conversation`）；同 `(parent_run_id, parent_tool_call_id)` 注册幂等。子工具集=父集合−task（文件工具按共享 `WorkspaceFileLocks` + 每 run 独立 `AgentReadState` 重建，其余复用父 Arc——schema 与派发精确等价）；子取消 token 是父的 `child_token()`。审批向父/子 hub 双向发布，decide 任一侧可路由、同一决定只生效一次；allow_always 注入整棵活动树。恢复：终态直接回填（不重跑子模型）、`lost` 同 run 行续跑（epoch+1，迟到实例写入被守卫拒绝）；存在未结算委派时 send/compact 409 `pending_recovery`。父子 loop 并发写库靠 `insert_message` 的 SQLITE_BUSY(5/517) 重试。
- **Agent 配置**（`agent_settings.rs` + `runtime/worker_config.rs`）: 内置 `worker`/`title` 全局→项目逐字段覆盖（`agent.{key}` JSON）；PATCH 遗漏=不改、null=恢复继承。worker 新 run 把指令/工具身份封存进 `agent_runs.agent_config_json`，恢复用原快照（NULL 兼容旧行）。
- **Repo instructions & 冻结 prompt 环境**（`agent/repo_instructions.rs` + `runtime/prompt_env.rs`）: 发现链=working_dir 沿祖先链到 git root 逐目录取 `AGENTS.md` → `CLAUDE.md` **首个命中**（每目录最多一个）；预算 32 KiB/文件、64 KiB 总量。块 + MCP 工具身份按根会话**冻结**（`RunService.prompt_envs`）保 provider prefix cache——只在首次装配、压缩/microcompact 之后、`POST /api/conversations/{id}/instructions/reload` 三个显式点刷新，run 中途恒定；子代理共享整棵树同一条目。故意不冻结：code mode 切换、附加目录、worker 指令。
- **Background bash** (`runtime/background.rs`): `run_in_background` **envelope 即答**；子进程输出 fd 直写 `{db目录}/exec/{conv}/{task_id}.log`（exec 目录为运行时只读附加根，任何权限模式下 write/edit 均拒绝；模型用通用 read/grep/glob 读取）。终态→等会话空闲→单事务插 `<task-notification>` user 消息 + 以会话上次 provider/model 起 wake run（不触发 title）；send/resume 入口 catch-up 补投。**不随 run 死**——只有 task_stop/HTTP cancel/会话删除两阶段杀（SIGTERM→1.5s→SIGKILL）；watchdog 1GiB 输出/24h 运行上限。
- **Agent tools** (`tools/`): `read`/`write`/`edit_file`/`listdir`/`grep`/`glob`/`execute`（含 `run_in_background`）/`task_stop`（仅 root toolset）。状态拆分：共享 `WorkspaceFileLocks`（跨 agent 串行）+ 每 run `AgentReadState`（阅读记录互不授权）；多根工作区沙箱解析在 `workspace`。
- **Code mode** (`src/codemode/` + `runtime/code_recovery.rs`): `mcp_tool_mode=code` 时 MCP 工具收进单一 `execute_code`——每 run 发布只读 SDK（`{db目录}/codemode/{conv}/{run}/sdk/`，模型用普通 read/grep/glob 浏览，`.js` 虚拟模块由 worker 提供）；宿主调用复用普通审批/文件锁/阅读状态；JS 无直接文件/网络/进程，仅 `text()`/`console.log()`/`image()` 显式输出进上下文。崩溃恢复从上个 run 的有界 trace 恢复嵌套调用状态（不重跑历史脚本）。代码模式下 worker 非空 allowlist 必含 read/grep/glob。限额/协议/完整规范见 `crates/oct-codemode/README.md`。
- **Permissions** (`src/permissions/`): 每次 model 驱动的工具调用过 `PermissionGate`——`permission_rules` + 会话 `permission_mode` 按固定优先级 **deny > ask > allow > mode 默认** 裁决（tool 精确名或 `mcp__{server}__*` 通配；pattern 为 NULL/命令前缀/绝对路径 glob）。Ask 停进内存 registry（oneshot、fail-closed 超时）；allow_always 带 pattern 落 source=`approval` 的持久规则；审批本身内存态不落库。后台任务输出目录内建 deny。
- **Microcompact** (`agent/micro.rs`): 摘要压缩前的廉价第一级——turn-top 把旧的成功态工具结果 **COW 替换**为占位符（副本 `supersedes_message_id` 回指原文恒一跳，绝不改原文）。触发：水位 ≥ 窗口×70%（compact 是 85%）或 run 启动时会话已空闲 ≥ 60min。永不清错误结果/最后一轮/已是占位符的行。`GET /api/conversations/{id}/messages/{copy_id}/original` 沿 supersedes 一跳取原文。
- **Context state** (`context.rs`): DB 与 agent 共享的中性层——token 估算（`estimate_*`）、`LlmView`/`UsageAnchor`、loop 运行态 `ContextState`（`append`/`apply_compaction`/`apply_clears` 是唯一变换入口，与重新 `load_llm_view` 保持一致）。
- **Context compaction** (`agent/compact.rs` + `db/messages.rs::load_llm_view`): 触发=turn 顶部「真实 usage 锚点+本地估算」≥ 窗口×0.85 或手动 `/compact`；手动/自动共用 `run_compaction`。关键不变式：标记行**插在切点槽位**（region 与 tail 之间——追加末尾会让 tail 落到 marker 之前、下个 run 从视图消失）；**老行永不删除**，视图=最新 marker 之后（孤儿 ToolResult 过滤）；anchor 须晚于 marker；尾部 verbatim 保留（窗口/10 封顶 32k，不拆 tool_call/result 对）；摘要失败只禁用自动、再触发需 est ≥ 压缩后估算 + tail 预算（防死循环）。
- **MCP support** (`src/mcp/` + `api/mcp.rs` + `oauth.rs`; 底层 crate 见 `crates/oct-mcp`): `McpManager` 每 chat turn 按项目对账 enabled `mcp_servers` 行（作用域两级合并）。并发模型：决策在短 `decide_lock` 内无网络等待完成，连接/移除各持该 server 的 gate；OAuth `invalidate` 推进失效 epoch 而非等在途握手（迟到结果不回写）。`tools/list_changed` 500ms 去抖重 list（`ManagedServer.generation` 防旧监听写新 entry）。工具超时默认 60s（行级覆盖），取消经 cancel_token select 中断在飞调用（不 shutdown 共享连接）。图片多模态：anthropic tool_result 变块数组，openai 兼容侧追加 user 消息带 data URL。OAuth=授权码+PKCE S256+RFC 8414/7591，token/secret 不进日志与 OpenAPI。

## API

路由全表以 `api/mod.rs` 的 `api_routes` 与 `/scalar`（OpenAPI）为准，只列**非显然语义**：

- Send 带 `expected_revision`（history 准备前校验）；`POST …/{id}/resume` 兼作子代理崩溃恢复入口（orchestrator 先重建任务树再续父 loop）；`GET …/{id}/run` + `GET …/{id}/events` 供前端刷新后接回在飞 run。
- **Fork**：切点=head / before-user / after-assistant / 闭合 tool 批之后（并行批不可拆）；共享内容行、复制 membership，**绝不**复制 runs/tasks/read grants；请求带 `operation_id`+`expected_revision`；切点丢弃其后的 marker、恢复其后被 COW 换掉的原文；以 tool 结果结尾的 fork 可无 run 恢复且不得重放历史工具。
- **Revert**：整段 pre-revert membership 存为只读备份（origin_kind='revert_backup'），单事务替换原时间线；context_revision/history_epoch 递增；活动受管任务/未通知后台任务/未完成 wake 阻塞 revert；备份会话拒绝 send/resume/compact/revert/权限变更（rename/delete/fork 可用）。
- `GET /messages` 同一读快照带 `X-Context-Revision` 与 `X-Context-Tokens`。

## Database Schema

列级定义以 `migrations/`（按时间序累加）与 `.sqlx/` 为准，只列**不变式**：

- `messages` 是**纯内容行、insert-only**（唯一后续写=usage 回填），跨 fork 共享；`conversation_messages` 是会话时间线：ordering 每会话唯一，`appended_at_revision` 插入时盖章永不变（fork/revert 切点与 anchor 资格判定用时点），`swapped_at_revision` 标记 COW 换指针。fork=复制行共享内容行，回滚=截断，microcompact COW=拨 message_id 指针（`apply_cow_swaps`）。
- fork/revert 元数据（`forked_from_conversation_id` 等、`conversation_fork_operations`/`conversation_revert_operations` 幂等收据）**不复用** subagent 的 parent/root 字段；`origin_kind`/`history_epoch` 区分世代（root latest/recovery 仅查当前世代）。
- `agent_runs` 根/子共用：`(parent_run_id, parent_tool_call_id)` 幂等键；`execution_epoch` crash 续跑递增、迟到实例写入被守卫拒绝；`agent_config_json` 封存 worker 快照；status `lost`=可恢复中断。
- `settings`/`project_settings` 是 KV：已知键归 API 层类型化 DTO（`llm_wire_log`、全局/项目 `mcp_tool_mode`、`agent.{key}`）。

## Testing

- CI 铁律：测试绝不调用真实模型，全部由本地 mock 驱动。
- `tests/support/` 基建（各集成测试文件共用）：`TestApp` 在随机端口起真实 axum 服务器 + 临时文件 SQLite（**不要 `:memory:`**——连接池各连接是独立的库）；LLM 是 `llm.rs` 的 OpenAI 兼容 mock（经 `POST /api/providers` 的 base_url 注入），`push_marker_turns` 按请求内容 marker 做多 agent 剧本路由（并行 tool call 需 `tool_call_chunk_at` 传不同 index）；另有 `anthropic_llm.rs`、`mcp.rs`（`FakeMcpConnection`，不起真 server）、`oauth_as.rs`（本地授权服务器）。
- 崩溃模拟统一手法：WAL checkpoint + db 文件复制 + `TestApp::new_with_dir`。
- 禁 sleep 等并发：取消用 `CancellationToken`，等事件用 `rx.recv()` + timeout 兜底，SSE 断言以流 EOF 为终止信号。
- 测试文件按主题拆分（`agent_loop`/`api_e2e`/`subagent_runtime`/`background_tasks`/`code_mode`/`conversation_fork`/`conversation_revert`/`git_api`/`mcp_oauth`/`agent_settings`/`tool_transactions`/`notification_history`/`*_recovery_*` 等）；`code_mode.rs` 起 true V8 helper 的用例在 `--features code-mode-tests` 之后（见 `crates/oct-codemode/AGENTS.md` 的 preserve_order 陷阱）。工具层为内联 `#[cfg(test)]` 测试。
