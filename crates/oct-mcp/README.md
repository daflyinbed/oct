# oct-mcp

MCP 客户端隔离层：用窄接口包装官方 MCP Rust SDK（[rmcp]），覆盖 oct 使用的两种传输——子进程 stdio 与 Streamable HTTP。

## 为什么存在

rmcp 的类型**绝不**进入公共 API：签名只说 `anyhow`、`serde_json`、std 和本 crate 自己的类型（如通知通道 `ToolListChanged`/`ToolListNotifier`）。工作区其余部分消费 MCP 工具时不需要直接依赖 rmcp，rmcp 升级的影响面被锁在这个 crate 里。

## 两半

- `src/connection.rs` — 启动 MCP server（stdio 拉起子进程，或 Streamable HTTP + 自定义 headers），跑 initialize 握手，然后经 `McpConnection` trait：list/call tools、list/render prompts、list/read resources、`tool_list_changed` 通知
- `src/naming.rs` — 纯函数：server 侧工具名 ↔ 平面名 `mcp__{server}__{tool}` 的映射（`qualified_tool_name`/`ToolNameMap`）、输入 schema 归一化（`normalize_input_schema`）、`MAX_TOOL_NAME_LEN`

## reqwest 0.13 铁律

Streamable HTTP 传输（feature `transport-streamable-http-client-reqwest`）会拉入 rmcp 的 reqwest 0.13，因此**整个工作区统一钉 reqwest 0.13**——所有 crate 一个版本。改动后用 `cargo tree -d -p reqwest` 验证必须报告无重复。

## Features

`oct-agent` 侧启用：`client` + `transport-child-process` + `transport-streamable-http-client-reqwest`。

## 测试

`tests/http_transport.rs`——本地 HTTP mock，不联系真实 server。McpManager（连接对账/OAuth/目录）与 REST 面的测试在 `crates/oct-agent`（见其 `AGENTS.md`）。

[rmcp]: https://crates.io/crates/rmcp
