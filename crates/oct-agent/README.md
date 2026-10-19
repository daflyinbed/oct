# oct-agent

oct 的后端服务 crate：Axum REST/SSE API + agent loop 运行时。既是 library（集成测试 import `oct_agent::…`），也产出 `oct-agent` 二进制。

## CLI 子命令

1. **`serve`**（默认）：启动 API 服务器。`--port` / `OCT_PORT`（默认 3000）
2. **`seed`**：从 <https://models.dev/api.json> 拉取模型目录 upsert 进数据库（source=`seeded`，自定义 provider/model 保留）。思考控制项数组（`reasoning_options`：effort / toggle / budget_tokens，可同时存在）与输入输出模态一并落库；发送消息时可带 `reasoning` 档位（`POST …/messages`），后端按模型值域校验并翻译成 wire 参数：Anthropic 使用 `thinking` 与 `output_config.effort`，OpenAI 兼容适配器目前仅支持 `reasoning_effort`（开关/预算会明确拒绝，界面不展示）。选择随真实 user 消息持久化，resume 会跳过压缩标记恢复；Anthropic 思考签名与脱敏块随历史保存，前缀绑定模型续传时按请求前缀指纹过滤失效块，不修改原始历史。MCP 工具结果的图片按 `modalities_input` 门控：模型不接受的图片在请求装配时替换为文本注记（存储与 UI 保留原图），列缺失/损坏放行一切

3. **对话文件上传**：聊天面板拖入文件即上传（图片与 UTF-8 文本，单文件 ≤ 10 MiB、单条消息 ≤ 8 个），chip 显示进度/失败重试。一次发送票据的认领与消息/资产引用原子提交，失败可重试；慢上传不占全局回收锁。图片完整解码校验后生成稳定的模型衍生图（最长边 2000、至多 3MiB，重编码时保留 EXIF 指定的显示朝向，GIF 首帧），下载仍返回原图。文本附件在发送时固定内联快照（窗口 10%，封顶 32k token、未知窗口 16k，单文件 256KiB），截断提示带逻辑资源引用，模型可用 read_file 读全文；搬迁附件目录不会改变快照。请求先做模态门控，再执行总媒体预算（应用默认 40MiB 编码字节/64 张），保留当前用户消息和最近工具轮，较旧图片可省略并注明；受保护图片超限则明确失败。UI 显示原始正文和附件链接，不展开模型快照。字节存 `attachments/`（目录 0700、文件 0600），共享内容行及其资产索引保留 fork/COW 原文/回退备份；最后引用消失后 sweep 回收。fork/revert 草稿签发新 staged 票据，共享同一不可变资产，源会话删除不影响已恢复草稿。

## 模块地图

- `api/` — REST + SSE 路由、utoipa OpenAPI（`/scalar` UI、`/api/openapi.json`）、嵌入前端（`embed.rs`）
- `agent/` — agent loop、上下文压缩（`compact.rs`）、microcompact（`micro.rs`）、repo instructions 与冻结 prompt 环境（`repo_instructions.rs`）
- `runtime/` — `RunService`（会话占用/run 持久化/装配/监督/崩溃恢复）、后台 bash 任务（`background.rs`）、worker 配置（`worker_config.rs`）、prompt 环境（`prompt_env.rs`）、code 恢复（`code_recovery.rs`）
- `subagent/` — `task` 工具委派的子代理运行时
- `permissions/` — 权限规则求值（rules/evaluator/descriptor/gate/registry）
- `tools/` — `read`/`write`/`edit_file`/`listdir`/`grep`/`glob`/`execute`/`task_stop`
- `codemode/` — code mode 宿主侧：catalog 生成、SDK 发布、诊断、storage
- `mcp/` — `McpManager`（连接对账/目录）、OAuth 凭据
- `db/` — sqlx 数据层（compile-time checked 查询）
- `context.rs` — token 估算与 loop 运行态 `ContextState`
- `reasoning.rs` — models.dev `reasoning_options` 的解析与用户档位校验
- `modality.rs` — models.dev `modalities_input` 的解析与图片（用户附件 + 工具结果）的请求侧门控
- `attachment_types.rs` — 中性正文/附件/固定文本快照契约与旧格式兼容读取
- `attachments/` — AttachmentService 生命周期、纯文件 AssetStore、共享 MediaNormalizer、请求物化 RequestAssembler；`context/projection.rs` 负责无 IO 的媒体选择与 usage 快照

## 数据库

SQLite，位置解析两级：`DATABASE_URL` 显式设置优先 → 默认 `$XDG_DATA_HOME/oct/oct.db`（目录 0700、库文件 0600）。迁移在仓库根 `migrations/`，启动时自动执行；查询全部走 `sqlx::query!` 宏（`.sqlx/` 离线缓存已提交）。改 SQL 后对 scratch 库跑迁移再 `cargo sqlx prepare --workspace`。

## 测试

`tests/` 全部集成测试，共享 `tests/support/`（`TestApp` 真实 axum 服务器 + OpenAI 兼容 LLM mock + FakeMcpConnection），CI 铁律：绝不调用真实模型。架构细节与各测试文件说明见本目录 `AGENTS.md`。
