# oct-mcp AGENTS.md

MCP 客户端隔离 crate。两条铁律：

- **rmcp 不出 crate**：rmcp 的类型绝不进公共 API——签名只说 `anyhow`、`serde_json`、std 和本 crate 自己的类型。给 oct-agent 加 MCP 能力时经 `McpConnection` trait，不是直接用 rmcp。
- **reqwest 全仓统一 0.13**：Streamable HTTP feature 拉 rmcp 的 reqwest 0.13，整个 workspace 因此钉同一版本。动依赖后 `cargo tree -d -p reqwest` 必须报无重复。

## 接口速览

- `connect_stdio`/`connect_http`（custom headers 透传 Authorization）、initialize 握手
- `McpConnection`：list/call tools、list/render prompts、list/read resources、`tool_list_changed` 通知通道（`ToolListChanged`/`ToolListNotifier`）
- `qualified_tool_name`（`mcp__{server}__{tool}`）/ `ToolNameMap` / `normalize_input_schema`
- features：`client` + `transport-child-process` + `transport-streamable-http-client-reqwest`

连接对账（`McpManager`）、OAuth、REST 面在 `crates/oct-agent`（见其 `AGENTS.md` 的 MCP support 节）；stdio 子进程的 Linux PDEATHSIG 由宿主侧统一设置。

## 测试

`tests/http_transport.rs`——本地 HTTP mock，不起真实 server。跨 crate 的 MCP 集成测试（manager 对账/OAuth/工具回路）都在 `crates/oct-agent/tests/`。
