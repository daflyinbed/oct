# oct-agent

oct 的后端服务 crate：Axum REST/SSE API + agent loop 运行时。既是 library（集成测试 import `oct_agent::…`），也产出 `oct-agent` 二进制。

## CLI 子命令

1. **`serve`**：启动 API 服务器。`--port` / `OCT_PORT`（默认 3000）。从 workspace 根运行 `cargo run -p oct-agent -- serve`。
2. **`seed`**：从 <https://models.dev/api.json> 拉取模型目录 upsert 进数据库（source=`seeded`，自定义 provider/model 保留）。从 workspace 根运行 `cargo run -p oct-agent -- seed`。

没有默认子命令，直接运行 `oct-agent` 会显示用法。

思考控制项数组（`reasoning_options`：effort / toggle / budget_tokens，可同时存在）与输入输出模态随模型目录一并落库；发送消息时可带 `reasoning` 档位（`POST …/messages`），后端按模型值域校验并翻译成 wire 参数：Anthropic 使用 `thinking` 与 `output_config.effort`，OpenAI 兼容适配器目前仅支持 `reasoning_effort`（开关/预算会明确拒绝，界面不展示）。选择随真实 user 消息持久化，resume 会跳过压缩标记恢复；Anthropic 思考签名与脱敏块随历史保存，前缀绑定模型续传时按请求前缀指纹过滤失效块，不修改原始历史。MCP 工具结果的图片按 `modalities_input` 门控：模型不接受的图片在请求装配时替换为文本注记（存储与 UI 保留原图），列缺失/损坏放行一切。

## 对话文件上传与读取范围

聊天面板拖入文件即上传（图片与 UTF-8 文本，单文件 ≤ 10 MiB、单条消息 ≤ 8 个），chip 显示进度/失败重试。一次发送票据的认领与消息/资产引用原子提交，失败可重试；慢上传不占全局回收锁。图片完整解码校验后生成稳定的模型衍生图（最长边 2000、至多 3MiB，重编码时保留 EXIF 指定的显示朝向，GIF 首帧），下载仍返回原图。文本附件在发送时固定内联快照（窗口 10%，封顶 32k token、未知窗口 16k，单文件 256KiB），截断提示带逻辑资源引用，模型可用 read_file 读全文；搬迁附件目录不会改变快照。请求先做模态门控，再执行总媒体预算（应用默认 40MiB 编码字节/64 张），保留当前用户消息和最近工具轮，较旧图片可省略并注明；受保护图片超限则明确失败。UI 显示原始正文和附件链接，不展开模型快照。字节存 `attachments/`（目录 0700、文件 0600），共享内容行及其资产索引保留 fork/COW 原文/回退备份；最后引用消失后 sweep 回收。fork/revert 草稿签发新 staged 票据，共享同一不可变资产，源会话删除不影响已恢复草稿。

运行时文件的额外读取授权按会话建立，并与项目工作目录及附加目录的读取范围取并集：后台输出开放当前会话目录，附件逐文件开放当前会话已发送的资产（含工具图片）；子代理继承根会话，fork 保留共享消息的附件授权。其他会话文件和未发送草稿的文件路径仍按普通工作区规则判断：工作区内默认可读；工作区外在 default/accept_edits 模式下默认需要审批，bypass 默认允许读取；显式权限规则仍优先生效。例如项目工作目录为用户主目录时，其下的默认运行时数据目录也属于工作区读取范围。

附件 URI 只按当前会话作用域的资源映射解析，不因文件位于工作区内而扩大解析范围；未在该映射中注册的 URI 返回 `Unknown attachment resource`。后台输出和附件的 write/edit 在所有权限模式下均被拒绝。

## 模块地图

- `application/conversation/` — send/resume/compact/wake、指令重载与恢复资格应用入口：校验、历史准备、附件认领、恢复编排与运行交接；HTTP 与后台调度共用
- `history/` — 无 SQL 的切点、工具批闭合、COW 恢复和草稿策略；`application/history.rs` 管 admission/占用，`db/history.rs` 在同一写事务内读快照、计划和提交；`api/history.rs` 仅负责公开 DTO 与 HTTP 转换
- `agent_settings/` — 配置继承、PATCH 与快照的中性类型/纯规则；`application/settings.rs` 提供编辑读取、运行读取和事务更新，`db/agent_settings.rs` 读写配置及引用事实，`api/agent_settings.rs` 转换公开 DTO 和 PATCH 三态
- `api/` — REST + SSE 路由、utoipa OpenAPI（`/scalar` UI、`/api/openapi.json`）、嵌入前端（`embed.rs`）
- `agent/` — `loop_runner.rs` 协调回合，`loop_runner/` 分别管理上下文维护、模型回合收集/保存、工具批次执行/结算；另有上下文压缩（`compact.rs`）、microcompact（`micro.rs`）、repo instructions 与冻结 prompt 环境（`repo_instructions.rs`）
- `application/assembly/` — 根/worker 装配器与共享权限、资源、冻结 prompt 环境；`application/execution.rs` 持有独立启动任务，`application/title.rs` 独立准备标题任务；code 恢复在 `assembly/code_recovery.rs`
- `application/services.rs` — 共享资源装配；HTTP state 持会话/历史/删除/权限应用命令和查询组件，运行材料只在应用层流转
- `application/lifecycle.rs` + `permissions.rs` — 会话/项目删除的执行 drain 与资源回收、活动权限门操作
- `runtime/` — 会话占用和 admission/tombstone；`execution.rs` 统一监督根/子 loop、终态持久化和收尾；`ModelResolver` 独立解析模型；另有后台进程管理（`background.rs`）与 worker 快照选择/恢复（`worker_config.rs`）
- `application/delegation.rs` + `inheritance.rs` — 原子委派注册、幂等与取消；父工具继承只保留冻结描述及 MCP 实例
- `application/child_startup.rs` — 子代理启动与恢复：只读准备、执行认领、装配、监督交接和失败结算共用一个 module；调用方取已结算结果，不持执行 future
- `application/notifications.rs` + `tool_ports.rs` — 通知聚合/wake 重试与按 run 绑定的后台能力；启动入口持有并关闭 drainer
- `subagent/` — 活动子代理索引、完成观察与持久结果信封；不回调应用服务
- `permissions/` — 权限规则求值（rules/evaluator/descriptor/gate/registry）
- `tool_contract/` — 中性工具身份、调用上下文、输出与 scoped ports；`tool_host/` — 权限派发与进度事件适配；`events/` — SSE 事件类型与内存 replay hub
- `tools/` — `read`/`write`/`edit_file`/`listdir`/`grep`/`glob`/`execute`/`task_stop`/`task`；原生工具只依赖契约和注入能力
- `terminal.rs` + `api/terminals.rs` — 项目级交互 shell（WezTerm `portable-pty`）；REST 创建/列出/关闭，WebSocket 收发原始字节与 resize。PTY 独立于浏览器连接，单会话只允许一个输入控制者，断线按字节游标重放最近约 1 MiB 输出。会话只驻留内存，最多 16 个；关闭终端/删除项目会终止 shell 与前台进程，服务重启后需重新创建。
- `codemode/` — code mode 宿主侧：catalog 生成、SDK 发布、诊断、storage
- `mcp/` — `McpManager`（连接对账/目录）、OAuth 凭据
- `application/git.rs` + `git/` — 项目仓库解析、共享仓库锁、Git 查询/操作、进程执行和输出解析；`api/git.rs` 保留公开 DTO 与 HTTP 适配
- `db/` — sqlx 数据层（compile-time checked 查询）
- `context.rs` — token 估算与 loop 运行态 `ContextState`
- `reasoning.rs` — models.dev `reasoning_options` 的解析与用户档位校验
- `modality.rs` — models.dev `modalities_input` 的解析与图片（用户附件 + 工具结果）的请求侧门控
- `attachment_types.rs` — 中性正文/附件/固定文本快照契约与旧格式兼容读取
- `attachments/` — AttachmentService 生命周期、纯文件 AssetStore、共享 MediaNormalizer、请求物化 RequestAssembler；`context/projection.rs` 负责无 IO 的媒体选择与 usage 快照

发送准备和根/子运行启动由独立任务负责；断开请求或丢弃等待 future 不会中止已经提交的准备工作。子代理由统一启动 module 从注册/恢复到监督接管持续拥有执行者；重复委派与会话恢复复用同一份预检和结算规则。取消父回合会通知启动中的子代理，子代理落到终态后释放会话占用；单独取消子代理也覆盖启动装配阶段。数据库中没有执行所有者的 `running` 子任务可直接取消，或在父委派仍未结算时恢复，无需重启服务。

## 数据库

SQLite，位置解析两级：`DATABASE_URL` 显式设置优先 → 默认 `$XDG_DATA_HOME/oct/oct.db`（目录 0700、库文件 0600）。迁移在仓库根 `migrations/`，启动时自动执行；查询全部走 `sqlx::query!` 宏（`.sqlx/` 离线缓存已提交）。改 SQL 后对 scratch 库跑迁移再 `cargo sqlx prepare --workspace -- --all-targets`。

消息接口的 `MessageDto` 直接提供 UTC 时间戳：用户消息（含后台通知）的 `started_at` 来自消息创建时间，助手回复的 `finished_at` 来自回复落库时间；压缩标记和工具结果不提供计时字段。前端按用户轮次取开始时间与最终回复的结束时间计算耗时，运行中本地走表，取消与失败只显示状态。恢复后的回复沿用原用户轮次，等待恢复和停机时间仍计入；fork/回滚共享的消息保留原时间。无需额外查询或聚合运行记录。

## 测试

`tests/` 全部集成测试，共享 `tests/support/`（`TestApp` 真实 axum 服务器 + OpenAI 兼容 LLM mock + FakeMcpConnection），CI 铁律：绝不调用真实模型。架构细节与各测试文件说明见本目录 `AGENTS.md`。

聊天重连以执行实例为边界：SSE 的 `run_meta` 给出 run/epoch 和持久化基底版本，消息接口给出不可变 membership 插入版本及快照 revision/history epoch。根会话和子代理使用同一协议，恢复后保留上次执行已持久化的工作。修改该协议时前后端需一起更新。
