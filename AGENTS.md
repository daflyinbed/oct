# AGENTS.md

## Monorepo Layout

Two separate package managers coexist:

- **Rust workspace** (`Cargo.toml`, toolchain 钉在 `rust-toolchain.toml` 1.99.0): `crates/oct-agent` (library + `oct-agent` binary; integration tests import `oct_agent::…`) 消费三个独立基础 crate：`crates/oct-llm-provider`（LLM library）、`crates/oct-mcp`（经窄 `McpConnection` trait 隔离 rmcp；stdio + Streamable HTTP，reqwest 全仓统一 0.13）、`crates/oct-codemode`（协议/宿主客户端；deno_core/V8 只在 `runtime` feature 编译的 `oct-codemode-worker` 辅助二进制里）。
- **pnpm workspace** (`pnpm-workspace.yaml`): `packages/app` (Vue 3 frontend)

**每个包有自己的 `AGENTS.md`（开发约定 + 架构深挖）与 `README.md`（用户文档）**——动某块代码前先读对应文件：

- `crates/oct-llm-provider/AGENTS.md` — provider 抽象、LLM wire 日志（tracing layer 陷阱）、insta/wire 测试
- `crates/oct-mcp/AGENTS.md` — rmcp 隔离铁律、reqwest 0.13 单版本铁律
- `crates/oct-codemode/AGENTS.md` — 协议常量、worker 构建/测试、preserve_order 测试陷阱（完整规范在其 `README.md`）
- `crates/oct-agent/AGENTS.md` — 应用入口/装配/执行边界、history/settings 纯策略与事务、agent loop、子代理/后台任务、API、DB 不变式与集成测试
- `packages/app/AGENTS.md` — 前端架构、单测/浏览器 e2e 约定、reka-ui

## Commands

### Rust (run from workspace root)

```bash
cargo build -p oct-agent              # build the backend binary
cargo build -p oct-agent --release    # 发布构建：先 `pnpm --filter frontend build`（见 Build & Database 的嵌入说明）
cargo run -p oct-agent -- serve        # start backend (port 3000 by default, override with OCT_PORT)
cargo run -p oct-agent -- seed         # seed providers & models from models.dev API into the database
cargo test --workspace                # ordinary workspace tests; do not enable oct-codemode/runtime here
cargo test -p oct-llm-provider         # run library tests (snapshot tests via insta)
cargo test -p oct-llm-provider --test providers_anthropic  # single integration test target
cargo sqlx prepare --workspace -- --all-targets  # regenerate .sqlx/ cache, including test queries
```

CLI 必须显式传 `serve` 或 `seed`；没有默认子命令。

### Code mode（V8 helper，默认工具编排方式）

```bash
cargo build -p oct-codemode --features runtime   # build oct-codemode-worker（默认不编译）
pnpm --filter frontend build                    # release/打包前先构建嵌入的界面
cargo build --workspace --features oct-codemode/runtime --release  # 部署：两组件一起构建
cargo test -p oct-codemode --features runtime    # worker 运行时测试
SQLX_OFFLINE=true cargo test -p oct-agent --features code-mode-tests --test code_mode  # 起真 helper 的集成测试
bash scripts/package-code-mode.sh                 # 使用已构建的前端打包 Rust 组件 + SHA256 校验
```

- `mcp_tool_mode` 全局默认是 **code**（`settings`/`project_settings` 表，设置页「工具调用方式」可改 direct；项目可覆盖/恢复继承）。
- worker 发现顺序：`OCT_CODEMODE_WORKER` 环境变量 → `oct-agent` 可执行文件同目录/相邻目录的 `oct-codemode-worker`。主会话组件不可用时**回退 direct** 并提示；子代理用封存快照，恢复时组件不可用则显式失败。
- **测试隔离**：runtime 测试与常规 workspace 测试分开跑——Deno 依赖会启用 `serde_json/preserve_order`，合并跑会让 insta 快照出现 JSON 键顺序差异（不要为此更新模型快照）。
- agent 的 `code-mode-tests` feature 只启用集成用例，不构建 worker；先构建 helper。自动发现不到时，运行测试前设置 `OCT_CODEMODE_WORKER` 为构建产物的绝对路径。
- 通信是 stdin/stdout 上的 4 字节大端长度 + JSON 帧（协议版本校验），目前仅在 Linux 验证；Windows 资源限制未接入。

### Frontend (run from workspace root with pnpm)

Node.js 版本用 vp 管理（`vp env current` 查看当前环境）；pnpm 版本跟随根 `package.json` 的 `packageManager`。

```bash
pnpm --filter frontend dev            # Vite dev server (port 5173, /api 代理到 3000)
pnpm --filter frontend build          # vue-tsc typecheck then vite build
pnpm --filter frontend generate-api   # regenerate TypeScript types from OpenAPI schema (requires running server)
pnpm --filter frontend test           # vitest 单测
pnpm --filter frontend test:browser  # 浏览器 e2e 拦截层（chromium；首次需 `pnpm --filter frontend exec playwright install chromium`）
```

### Lint

```bash
pnpm -r exec eslint .                 # ESLint with @xwbx/eslint-config flat config
```

## Build & Database

- **Database**: SQLite. 位置解析（`db::resolve_database_url`）仅两级：`DATABASE_URL` 显式设置优先 → 默认 `$XDG_DATA_HOME/oct/oct.db`（Linux `~/.local/share/oct/`，目录 0700、库文件 0600——存 API key/OAuth token；data dir 解析不出则启动报错）。dev 想把库放进 checkout 就显式 `DATABASE_URL=sqlite://oct.db cargo run -p oct-agent -- serve`。仓库 `.env` 是纯编译期配置（sqlx 宏自行加载：`SQLX_OFFLINE` 走 `.sqlx` 缓存、`DATABASE_URL` 给 `cargo sqlx prepare` 的仓库根 scratch 库），运行时不加载。`exec/`、`llm-wire/`、`codemode/`、`attachments/` 自动跟 db 目录走（`OCT_EXEC_DIR`/`OCT_ATTACHMENT_DIR` 可单独改）
- **sqlx compile-time macros**: All queries use `sqlx::query!`/`query_as!`/`query_scalar!` (compile-time checked, `sqlite` feature)
- **Offline mode**: `.sqlx/` cache is committed; builds work without a live database via `SQLX_OFFLINE=true`
- **Regenerate cache**: After changing SQL queries, run migrations against a SQLite database and then run `cargo sqlx prepare --workspace -- --all-targets`
- **Migrations**: `sqlx::migrate!("../../migrations")` in `crates/oct-agent/src/db/mod.rs` auto-runs on startup; migration files live at repo root `migrations/`
- **Seeding**: `cargo run -p oct-agent -- seed` fetches the model catalog from `https://models.dev/api.json` and upserts providers/models (source=`seeded`). Custom providers/models (source=`custom`) are preserved.
- **Embedded frontend**: `packages/app/dist` 经 rust-embed（`api/embed.rs`，tower-http 0.7 `ServeDir::with_backend` + SPA fallback，history 路由深链回 `index.html`、未知 `/api`/`/scalar` 路径保持 404）嵌进 `oct-agent` 二进制——release 编译期烧入（**构建顺序：先 `pnpm --filter frontend build` 再 `cargo build --release`**，dist 缺失/过旧会静默嵌入空表/旧产物，启动时 warn 提示）；debug 构建不嵌入、运行时读 dist 目录（配合 vite dev server 5173 走代理）。`allow_missing` 保证从未构建前端的 checkout 也能纯 Rust 编译。
- **LLM wire 日志**: 开关与目录的 env/设置见根 `README.md` 配置表；实现与 tracing layer 陷阱在 `crates/oct-llm-provider/AGENTS.md`。

## Docs

- `docs/`（调研笔记 `docs/explore/`、`docs/backlog.md`）**整体不入库**（.gitignore），只存在于本机——fresh checkout 里没有，别去找。历史架构决策记录（ADR 0001–0004）已删，见 git 历史。Code mode 的完整说明在 `crates/oct-codemode/README.md`。

## Testing

- CI 铁律:测试绝不调用真实模型。所有 LLM 交互都由 mock 驱动(scripted model / 本地 HTTP mock server)。
- 各包的测试基建与陷阱见对应 `AGENTS.md`（见 Monorepo Layout 的清单）。
- oct-agent 集成测试的公共规则：禁 sleep 等并发（取消用 `CancellationToken`，等事件用 `rx.recv()` + timeout 兜底；SSE 断言以流 EOF 为终止信号）；DB 用临时文件 SQLite，不要 `:memory:`。

## Code Style

- **Comments**: 除非用户明确要求，否则不要删除代码中的注释；但可以更新已有注释以保持准确性
- **Rust edition 2024**: All four crates use `edition = "2024"`
