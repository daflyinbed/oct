# oct-codemode

代码模式的协议与运行时 crate。模型在沙箱 TypeScript（deno_core）里组合调用宿主工具：V8 只活在独立的 `oct-codemode-worker` 辅助进程里，宿主（oct-agent）与 worker 通过 stdin/stdout 上的长度前缀 JSON 帧通信。

- `src/protocol.rs` — 帧格式与全部限额常量（协议版本 3）
- `src/client.rs` — 宿主侧：worker 发现/启动、rlimit/PDEATHSIG 设置、`Host` trait（call/emit/image）
- `src/worker.rs` — `oct-codemode-worker` 二进制（`runtime` feature 才编译）：单 isolate，stdio 由 Rust 持有、不暴露给 V8

工具目录生成、SDK 发布、诊断与崩溃恢复等宿主侧集成在 `crates/oct-agent/src/codemode/`（`catalog`/`diagnostics`/`storage`）与 `crates/oct-agent/src/runtime/code_recovery.rs`。

## 构建与部署

以下命令均在 workspace 根执行：

```bash
cargo build -p oct-codemode --features runtime    # 构建 oct-codemode-worker（默认不编译）
cargo build --workspace --features oct-codemode/runtime --release  # 部署：两组件一起构建
bash scripts/package-code-mode.sh                 # release 打包（组件 + SHA256 校验）
```

代码模式可在「设置 → AI → 工具调用方式」选择，全局默认代码模式（显式选「直接调用」或项目覆盖可改回），项目可覆盖或恢复继承。切换在下次运行生效，支持主会话和 Worker 子代理的文件、bash、MCP 工具编排。持久化字段为 `mcp_tool_mode`，兼容现有配置。

`oct-codemode-worker` 需要与 `oct-agent` 放在同一个目录，也可通过 `OCT_CODEMODE_WORKER=/absolute/path/oct-codemode-worker` 指定。主会话组件不可用时会回退到直接调用，界面显示提示；子代理使用已封存的运行快照，恢复时组件不可用会明确失败。通信使用标准 stdin/stdout 管道，当前已在 Linux 验证；其它平台尚未验证，Windows 的操作系统资源限制尚未接入。

## 模型如何使用

每次主会话运行会在数据库目录旁的 `codemode/{conversation}/{run}/sdk/` 生成只读 SDK：`index.json`、`builtin/tools.d.ts`、按 MCP server 分组的 `.d.ts`、原始 schema 和使用说明。Worker 在自己的会话/run 下使用 `sdk-{execution_epoch}/`，恢复时保留旧目录并生成新目录。超过 24 个工具或 32 KiB 声明的 server 自动拆为短索引 + 每工具一个声明/schema 文件；`toolIndex` 列出原始工具名、导出名和文件路径。声明使用多行类型，避免单行预览截断。相同身份、描述和 schema 的生成结果复用有界内存缓存（64 MiB / 8 个 catalog），每个 run 的实际工具和授权仍独立绑定。

系统提示词会列出已加载的 server 分组和 SDK 路径，模型用已授权的 `read_file` / `grep` / `glob` 浏览 SDK（读 `index.json` 拿 import 说明符，再按需读 `.d.ts` 声明与 README），最后用 `execute_code` 执行 TypeScript。SDK 目录挂在运行的只读根下：只读、写入一律拒绝，读取也不授予编辑权限；声明中的 `.js` import 由运行时提供，不需要在工作区安装 Deno、Node 或 npm 包。TypeScript 仅转译，调用参数由主进程按原始 JSON Schema 校验。

例如在一个脚本中读文件、执行命令并只输出所需摘要（参数以当前声明为准）：

```ts
import { tool_read_file, tool_execute_command } from "./builtin/tools.js";
const file = await tool_read_file({
  path: "Cargo.toml",
  start_line: 1,
  end_line: 20,
});
const command = await tool_execute_command({ command: "git status --short" });
text({ file: file.text, status: command.text });
```

## 脚本环境

脚本使用全新的 `deno_core` VM，通过受控桥接调用实际工具，每次调用复用普通审批。文件工具沿用工作区解析、共享文件锁和本代理的阅读记录；直接调用与脚本调用共用这些状态，Worker 的阅读记录独立。原生工具返回现有有界文本预览，`read_file` 含行号，长文件需分段读取；MCP 保留预算内的原始文本和结构化结果。中间结果留在脚本中，只有 `text(value)` / `console.log(value)` / `image({mimeType,data})` 的显式输出进入模型上下文。支持 top-level await；JS 不提供直接文件、网络、进程、任意 module import 或跨调用的变量状态；文件/bash 操作只能经过宿主工具。

Worker 从父级实际工具集按现有 allowlist 选工具，其 SDK 只包含选中的接口；代码模式下非空 allowlist 必须包含 `read_file` / `grep` / `glob`（配置接口直接拒绝，模式翻转后已封存的配置在 spawn 时判定 Failed，不会拿到盲的 `execute_code`）。空 allowlist 仍不提供任何工具。调用方式、catalog 版本和工具身份写入 Worker 配置快照，恢复不受后续设置变更影响，旧快照继续直接调用。`task` 保留为外层工具，脚本不能递归委派；Worker 的后台 bash 继续禁用。主会话脚本可调用后台 `execute_command` 和 `task_stop`，后台任务归属于外层 `execute_code` 时间线，继续使用已有输出文件、停止和通知机制。

## 协议与限额

宿主向 helper 的 stdin 写入协议帧，从 stdout 读取协议帧（4 字节大端长度 + JSON，单帧最多 32 MiB）。JS 的 console/text/image 均通过 Rust bridge 封装成帧，不直接写 stdout；stderr 丢弃，错误通过协议返回。启动时校验协议版本，修改通信方式后应一起重建两个组件。宿主关闭 stdin 后，helper 的独立读取线程立即结束进程，JS 同步死循环也不能阻止回收。

执行限额：最多 4 个脚本同时运行；每个脚本最多 128 次工具调用、8 次并发，默认 120 秒（审批等待不计入，可指定 1–300000 ms，最长总生存期 15 分钟）。V8 堆上限 256 MiB；Unix 使用 RLIMIT_CPU，累计 CPU 时间软限额 10 秒、硬限额 11 秒，并关闭 core dump。Linux 父进程每 25 ms 读取 /proc 的 RSS，超过 512 MiB 后终止 helper；这是采样监测，可能短暂超限，不是内核硬内存限额。等待工具不等于消耗 CPU 时间，审批等待也不会暂停 CPU/RSS/总生存期保护。Linux 额外用 PDEATHSIG 防止宿主退出后遗留 helper，其它平台靠 stdin EOF 回收。

代码最多 256 KiB、工具 selector 4 KiB、单次参数 1 MiB，所有待完成调用的参数和 selector 合计最多 8 MiB；超出待完成调用预算会抛出可捕获的 `ToolCallError`，应先 await 已提交的调用。单次返回（包括错误，按 JSON 序列化大小计算）最多 8 MiB、显式文本输出 16 MiB；模型收到最多 32 KiB 文本预览。除 timeout_ms 外，以上配额目前为 `src/protocol.rs` 的代码常量。

## 嵌套调用错误

嵌套调用失败抛出可捕获的 `ToolCallError`：`code` 区分参数、权限、MCP 错误、超时、传输和结果超限，`result` 在 MCP 返回失败数据时保留原始 text / structuredContent / images。调用记录保存开始时间、耗时和错误码。

## 崩溃恢复

后端重启后的 send/resume 从当前会话最后一个 run 的有界 trace 恢复已完成/失败/未知状态和部分输出；路径、调用归属和源码均校验，不执行历史源码，缺失记录仍使用普通中断结果。恢复的 worker 保留 run id 与 cell ledger，但使用新的 `sdk-{epoch}/` 目录。

## 诊断与保留

源码、输出及嵌套调用记录保存在 `cells/{call-id-hash}/`，输出和调用记录即时落盘。取消不撤销已经发生的文件、命令或远端操作，恢复不会自动重跑历史脚本；记录中未完成的调用应当视为结果未知。诊断文件按运行保留 7 天，总预算 1 GiB、每运行 128 MiB（并发写入预留额度），活动目录不参与清理，删除会话时清理对应文件。

## 测试

验证代码模式（全部使用本地 mock，不访问真实模型或 MCP 服务）：

```bash
cargo test -p oct-codemode --features runtime
SQLX_OFFLINE=true cargo test -p oct-agent --features code-mode-tests --test code_mode
pnpm --filter frontend test
pnpm --filter frontend test:browser
```

运行时测试与常规 workspace 测试分开执行：Deno 的依赖 feature 会启用 `serde_json/preserve_order`，合并运行时 feature 的 workspace 快照比对会出现 JSON 键顺序差异；无需更新原有模型快照。

## 手动 probe

可手动读取现有 OpenCode 或 ZCode 配置并验证真实 MCP（example 在 oct-agent，从 workspace 根运行）；该 example 不会在测试中自动执行，只联系显式选择的 server，配置文件保持原样。省略 `--call` 时只获取工具目录；指定时经 `execute_code` 调用这一工具并只输出结果大小和字段名：

```bash
SQLX_OFFLINE=true cargo run -p oct-agent --example code_mode_probe -- \
  --config "$HOME/.config/opencode/opencode.jsonc" --server chrome-devtools \
  --output /tmp/oct-chrome-probe --call list_pages
SQLX_OFFLINE=true cargo run -p oct-agent --example code_mode_probe -- \
  --config ../arknights-rev/.zcode/config.json --server ida-pro \
  --output /tmp/oct-ida-probe --call idb_list
```

`--output` 必须是新目录；禁用的 server 需要显式 `--include-disabled`。仓库中的 Chrome DevTools / IDA schema fixture 仅包含服务公开的工具定义，CI 用 mock 运行，不访问真实服务或真实模型。
