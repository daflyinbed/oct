# oct-codemode AGENTS.md

代码模式 crate：协议 + worker 运行时。完整规范（SDK 结构、限额明细、恢复语义、probe）在 `README.md`；宿主侧集成（catalog/SDK 发布/诊断/恢复）在 `crates/oct-agent/src/codemode/`——改接口时两边要一起看。

## 命令（从 workspace 根）

```bash
cargo build -p oct-codemode --features runtime   # 构建 oct-codemode-worker（默认不编译）
cargo test -p oct-codemode --features runtime    # worker 运行时测试
cargo build --workspace --features oct-codemode/runtime --release  # 部署：两组件一起构建
bash scripts/package-code-mode.sh                # release 打包（组件 + SHA256 校验）
```

## 关键约定

- **限额与帧格式全部是 `src/protocol.rs` 的协议常量**（协议版本 3）：改通信方式后两组件必须一起重建（启动时校验协议版本）。
- **测试隔离**：runtime 测试与常规 workspace 测试分开跑——Deno 依赖会启用 `serde_json/preserve_order`，合并跑会让 oct-llm-provider 的 insta 快照出现 JSON 键顺序差异（不要为此更新模型快照）。
- worker 发现顺序：`OCT_CODEMODE_WORKER` 环境变量 → `oct-agent` 可执行文件同目录/相邻目录的 `oct-codemode-worker`。宿主侧不可用时的回退/失败语义由 oct-agent 决定。
- `McpToolMode` 的 `#[default]` 必须保持 `Direct`：v1 WorkerSnapshot 行缺 `tool_mode` 字段、走 serde 默认值解码，翻转成 Code 会拒绝所有 pre-code-mode 快照。运行时默认值在 `oct-agent` 的 `mode()` 里，不在这里。
- 平台：通信路径仅在 Linux 验证；Windows 的 OS 资源限制（RLIMIT_CPU/RSS/PDEATHSIG）未接入。
- V8 的非-Send 运行时只活在 worker 进程里；宿主侧通过 `client.rs` 的 `run()` + `Host` trait（call/emit/image）驱动，帧是 stdin/stdout 上 4 字节大端长度 + JSON。
