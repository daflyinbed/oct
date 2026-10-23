# oct

本地优先的 AI 编码代理：Rust 后端（`oct-agent`）+ Vue 3 前端，单个二进制同时提供 API 与界面。

## 功能

- **多 provider**：Anthropic、MoonshotAI（Kimi）与任意 OpenAI 兼容端点（可运行时注册自定义 provider）
- **MCP 客户端**：stdio + Streamable HTTP 传输、tools/prompts/resources、OAuth（授权码 + PKCE）、`tools/list_changed` 热更新
- **代码模式**（默认工具编排方式）：模型在沙箱 TypeScript（独立 V8 worker 进程）里组合调用文件/bash/MCP 工具，中间结果不进上下文——完整规范见 [crates/oct-codemode/README.md](crates/oct-codemode/README.md)
- **子代理**：`task` 工具把自包含子任务委派给独立子会话，复用同一运行时
- **权限与审批**：全局/项目级 allow/deny/ask 规则 + 会话模式（default/accept_edits/bypass），Ask 暂停等待人工决定，allow_always 沉淀为持久规则
- **上下文管理**：microcompact（工具结果占位符清除）+ 摘要压缩两级，水位自动触发，窗口占用实时可见
- **历史分支**：对话 fork（多切点）与 revert（整段回滚 + 只读备份）
- **后台任务**：`run_in_background` 交给独立任务运行时，完成后唤醒会话
- **运行时韧性**：run 占用监督、崩溃恢复（lost run 续跑/终态回填）、审批/事件跨刷新重连（SSE）
- **LLM wire 日志**：每个 LLM HTTP 交互的原始 wire 记录按对话落盘，可开关

## 快速开始

前置：Rust（`rust-toolchain.toml` 钉 1.99.0）、Node.js 与 pnpm（用 vp 管理环境，pnpm 版本见根 `package.json`）。

```bash
# 后端（serve，默认 3000 端口）
cargo run -p oct-agent -- serve

# 首次可从 models.dev 拉取 provider/model 目录
cargo run -p oct-agent -- seed

# 可选：构建代码模式 worker（默认工具编排方式需要；放 oct-agent 同目录即可）
cargo build -p oct-codemode --features runtime

# 前端开发（5173 端口，/api 代理到 3000）
pnpm install
pnpm --filter frontend dev
```

发布构建（前端烧入二进制，history 路由 SPA fallback 兜底；**必须先前端后 release**）：

```bash
pnpm --filter frontend build
cargo build -p oct-agent --release                     # 不含 code mode worker
cargo build --workspace --features oct-codemode/runtime --release  # 含代码模式 worker
```

CLI 需要显式子命令；例如 `oct-agent serve --port 3001`。`scripts/package-code-mode.sh` 构建并打包 Rust 组件，运行它之前也须先构建前端。

## 配置

| 环境变量                 | 作用                                                                      |
| ------------------------ | ------------------------------------------------------------------------- |
| `DATABASE_URL`           | SQLite 位置（默认 `$XDG_DATA_HOME/oct/oct.db`，目录 0700、库文件 0600）   |
| `OCT_PORT`               | serve 端口（默认 3000）                                                   |
| `OCT_CODEMODE_WORKER`    | 显式指定 `oct-codemode-worker` 路径（默认在 oct-agent 同目录/相邻目录找） |
| `OCT_EXEC_DIR`           | 后台任务输出目录（默认 db 目录下 `exec/`）                                |
| `OCT_ATTACHMENT_DIR`     | 聊天附件目录（默认 db 目录下 `attachments/`）                             |
| `OCT_LLM_WIRE_LOG=off`   | LLM wire 日志进程级硬关                                                   |
| `OCT_LLM_WIRE_DIR`       | wire 日志目录（默认 db 目录下 `llm-wire/`）                               |
| `OCT_LLM_WIRE_FIELD_MAX` | 单字段截断上限（0 = 关闭）                                                |
| `SQLX_OFFLINE=true`      | 编译用 `.sqlx/` 缓存（不连库）                                            |

仓库 `.env` 是纯编译期配置（sqlx 宏加载），运行时不读取。`exec/`、`llm-wire/`、`codemode/`、`attachments/` 运行时产物自动跟 db 目录走。

## 结构

```
crates/
  oct-llm-provider/   LLM provider 抽象、适配器、wire 日志
  oct-mcp/            MCP 客户端隔离层（rmcp 不出 crate）
  oct-codemode/       代码模式协议与 V8 worker（oct-codemode-worker）
  oct-agent/          后端：应用用例/装配、执行监督、工具契约、历史/设置策略、API 与 DB
migrations/           SQLite 迁移（启动自动执行）
packages/app/         Vue 3 + Vite + UnoCSS 前端
```

各 crate 和前端包有自己的 `README.md`（用户文档）与 `AGENTS.md`（开发约定与架构深挖）。后端模块归属见 [oct-agent 模块地图](crates/oct-agent/README.md#模块地图)，前端开发说明见 [packages/app/README.md](packages/app/README.md)。API 文档在运行中的 `/scalar`，OpenAPI JSON 在 `/api/openapi.json`。

## 测试

CI 铁律：测试绝不调用真实模型——所有 LLM 交互由本地 mock 驱动。

```bash
cargo test -p oct-llm-provider          # insta 快照 + wire 格式
cargo test -p oct-agent                 # API e2e / agent loop / 子代理 / 后台任务 / code mode…
cargo test -p oct-codemode --features runtime   # worker 运行时（与 workspace 快照测试分开跑）
cargo build -p oct-codemode --features runtime  # 先构建 helper，再跑下方 agent 集成测试
SQLX_OFFLINE=true cargo test -p oct-agent --features code-mode-tests --test code_mode
pnpm --filter frontend test             # vitest 单测
pnpm --filter frontend test:browser     # 浏览器 e2e 拦截层（首次需 playwright install chromium）
```

`code-mode-tests` 不负责构建 helper；测试进程自动发现不到时，设置 `OCT_CODEMODE_WORKER` 为 worker 的绝对路径。

开发约定（命令、构建顺序、测试陷阱、架构细节）见根目录与各包的 `AGENTS.md`。
