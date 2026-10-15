# oct-agent

oct 的后端服务 crate：Axum REST/SSE API + agent loop 运行时。既是 library（集成测试 import `oct_agent::…`），也产出 `oct-agent` 二进制。

## CLI 子命令

1. **`serve`**（默认）：启动 API 服务器。`--port` / `OCT_PORT`（默认 3000）
2. **`seed`**：从 <https://models.dev/api.json> 拉取模型目录 upsert 进数据库（source=`seeded`，自定义 provider/model 保留）。思考控制项数组（`reasoning_options`：effort / toggle / budget_tokens，可同时存在）与输入输出模态一并落库；发送消息时可带 `reasoning` 档位（`POST …/messages`），后端按模型值域校验并翻译成 wire 参数：Anthropic 使用 `thinking` 与 `output_config.effort`，OpenAI 兼容适配器目前仅支持 `reasoning_effort`（开关/预算会明确拒绝，界面不展示）。选择随真实 user 消息持久化，resume 会跳过压缩标记恢复；Anthropic 思考签名与脱敏块随历史保存，前缀绑定模型续传时按请求前缀指纹过滤失效块，不修改原始历史

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

## 数据库

SQLite，位置解析两级：`DATABASE_URL` 显式设置优先 → 默认 `$XDG_DATA_HOME/oct/oct.db`（目录 0700、库文件 0600）。迁移在仓库根 `migrations/`，启动时自动执行；查询全部走 `sqlx::query!` 宏（`.sqlx/` 离线缓存已提交）。改 SQL 后对 scratch 库跑迁移再 `cargo sqlx prepare --workspace`。

## 测试

`tests/` 全部集成测试，共享 `tests/support/`（`TestApp` 真实 axum 服务器 + OpenAI 兼容 LLM mock + FakeMcpConnection），CI 铁律：绝不调用真实模型。架构细节与各测试文件说明见本目录 `AGENTS.md`。
