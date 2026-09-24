# Subagent 设计提案：独立会话、统一运行服务、分阶段协作

- 状态：提案，尚未实施
- 日期：2026-09-24
- 依据：[六家机制调研](../explore/ai-agent-subagent-mechanisms.md)与当前 OCT 源码；外部实现按调研记录的版本比较，不代表重新核验了各仓库最新版本。

## 1. 建议

采用「独立子会话 + 复用 agent loop + 轻量任务监督器」。第一版 `task` 工具默认等待结果，可通过同一轮的多个 tool call 并行派发。子代理默认空白对话、继承项目环境与父模型，最多一层；只提供 `worker` 一个角色，暴露父 agent 当前可用的全部工具（包括 MCP），唯一排除 `task`。历史存 SQLite，运行句柄留在内存。

实现顺序是：先统一运行服务，再完成 worker 与共享工作区的执行闭环，最后交付主 agent 任务卡片、与主聊天面板共用展示组件的子 agent panel，以及底部 subagent 指示器。三步共同构成第一版；后台运行和通信后续按需增加，不安排 explorer 先行阶段。

第一版适合「并行找代码、分头定位问题、委派明确的修改」。如果目标是「父代理一边推理/改代码，一边不断指挥子代理」，则必须推进后台阶段；同步 task 的并行能力不能替代这种协作。

## 2. 六家的关键取舍

| 方案 | 值得借鉴 | OCT 暂不采用的部分 |
|---|---|---|
| kimi-cli | task 内等待、独立上下文、限制递归，最短路径完成委派 | 以摘要长度判断质量；前后台各有一套生命周期 |
| opencode | 子会话作为一等持久对象，前后台共享任务运行实体 | 第一版就做 promote；运行期拒绝但仍暴露被禁工具的 schema |
| grok-build | 明确状态、准入、取消、结构化结果信封 | steer/queue/wake、workflow、角色/persona/多来源配置同时铺开 |
| Codex | 执行面与控制面分开；区分发消息与启动下一轮 | 团队路径寻址、驻留 LRU、消息板、分布式后端接缝 |
| Claude Code | 复用同一个循环，普通子任务与 fork 分开，可选 worktree | 定时转后台及多种 agent/team 模式；照搬其复杂权限优先级 |
| oh-my-pi | 对结果形状有明确约定，隔离写任务有独立产物 | 强制 yield 与纠正轮、IRC 总线、自动合并工作区 |

比较时必须分开四个维度：

1. **独立上下文与文件隔离**：所有实现可以有独立会话；只有部分支持可选 worktree。共享目录并非 Codex 独有。
2. **异步执行与后台存活**：子运行可以是独立 Tokio task，但父工具仍等待；立即返回也不意味着取消父任务后子任务继续存活。
3. **等待超时与执行超时**：前者只是本次读取返回，后者终止子运行。
4. **通知 UI 与通知模型**：前者是 SSE/状态查询，后者必须进入下一次模型请求；仅发事件不会让模型知道结果。

## 3. 当前代码可以复用什么

| 现有能力 | 对设计的影响 |
|---|---|
| [`run_agent_loop`](../../crates/oct-agent/src/agent/loop_runner.rs) 接受独立 `AgentContext`、conversation id 与 `LlmView` | 子代理直接复用生产循环、工具执行、压缩与持久化，不另写递归 LLM 调用器 |
| loop 使用 `FuturesUnordered` 执行同轮工具 | 多个等待型 task 可以并行；父模型要等整批工具完成后才能继续采样 |
| [`start_agent_run`](../../crates/oct-agent/src/api/chat.rs) 负责模型/工具装配和后台启动 | 目前与 HTTP/SSE 绑定，先抽成应用层 RunService，工具不能调用 REST handler 来启动子代理 |
| [`ToolContext`](../../crates/oct-agent/src/tools/mod.rs) 已带取消令牌、call id | task 可绑定父调用及取消链；身份、角色、根任务范围由宿主注入，不能由模型填入 |
| [`EventHub`](../../crates/oct-agent/src/agent/events.rs) 支持内存重放，释放所有 hub 引用后 EOF | 子运行各用一个 hub；监督器只在活动期持有句柄，终态仅保留数据，避免 SSE 永不结束 |
| [`messages`](../../crates/oct-agent/src/db/messages.rs) 已分离内容与 membership，并有 COW/压缩视图 | 直接复用子会话存储；这只是未来 fork 的基础，当前没有完整 fork API/快照语义 |
| [`ToolSharedState`](../../crates/oct-agent/src/tools/shared.rs) 将文件锁和阅读记录放在一起 | worker 上线前拆分：工作区共享锁，各 agent 私有阅读记录 |
| [`useChat`](../../packages/app/src/composables/useChat.ts) 使用模块级单会话状态 | 子任务面板不能再调用它来加载另一条流；需独立实例状态和订阅所有权 |

另有两点要补：loop 目前返回 `()`，不能可靠地直接拿到终态结果；当前 [`system_prompt`](../../crates/oct-agent/src/agent/prompt.rs) 静态列出全部内置工具，也没有这里假定的 AGENTS.md 自动装配。父子提示词应按实际工具清单生成，子提示词不包含 task 的调用指引；不要声称子代理已经继承了尚未实现的项目指令加载能力。

## 4. 第一版对模型的接口

```json
{
  "name": "task",
  "arguments": {
    "description": "定位 SSE 重连后工具卡片重复的原因",
    "prompt": "检查 useChat 与 EventHub 的重放路径，返回原因、文件位置和修改建议。"
  }
}
```

第一版仅暴露 `description` 和 `prompt` 两个参数，角色由宿主固定为 worker，不要求模型传 `agent_type`。不暴露后台开关、任意 cwd、角色文件路径、完整历史 fork 或模型覆盖。`description` 用于 UI 标题；`prompt` 负责目标、必要背景、文件范围与验收要求。

返回由宿主生成的信封，正文取该 run 的最终 assistant 文本：

```json
{
  "agent_id": "<child-conversation-id>",
  "run_id": "<child-run-id>",
  "status": "completed",
  "output": "原因、涉及文件、验证结果……",
  "truncated": false,
  "usage": { "input_tokens": 12000, "output_tokens": 900 }
}
```

- `completed` 表示循环正常结束，不保证任务结论正确。失败、取消有独立终态和错误码；进程丢失记为可恢复的 `lost`，不直接作为 task 失败交给父模型。
- 中断时可返回已持久化的部分输出，但必须标 `partial`，不能将最后一条进度说明当成成功最终答复。
- 返回文本复用工具输出截断预算，保留完整子历史。第一版 UI 可看全文；模型若收到截断结果，只能根据摘要继续或重新委派，分页读取工具留到后台阶段。
- `ToolOutput.details` 放 UI 元数据；模型需要的状态、摘要、错误必须进入 `content`，不能只藏在 details 中。
- 提示子代理报告结论、证据/修改文件、测试、未解决项；不强制 yield，不因字数短而追加模型轮次。
- 子代理异常作为 task 工具错误交给父代理处理，其他兄弟继续；取消父 run 才级联取消全部子运行。

## 5. 上下文、能力与工作目录

### 5.1 默认独立上下文

子会话首条消息是委派 prompt，不自动复制父历史。继承 spawn 时的项目目录快照、附加目录、有效模型及模型窗口，复用同一 provider 解析逻辑；首版模型固定继承父模型。以后再增加「调用覆盖 → 角色配置 → 父模型」的解析链。

系统提示词由公共基础、角色指令、委派身份和实际工具清单组成。后续若加入项目指令加载，父子共用同一装配器。默认全量 fork 会放大 token、混入父的分工指令，也会牵涉压缩 marker、未完成 tool call、usage anchor；不作为第一版捷径。全新上下文也不保证总 token 更低：父代理仍需提供足够背景，子代理可能重复读文件。

### 5.2 只做 worker，唯一排除 task

worker 统一承担代码定位、分析、实现、修复与测试，由委派 prompt 描述任务，不再区分 explorer/plan 等子角色。

**有效工具 = spawn 时父 agent 当前可用工具集合 − `task`。** 包括全部已启用内置工具和项目作用域内父 agent 可用的 MCP 工具，不额外设置 worker 专属白名单或禁用 shell/MCP。以后新增的父工具默认也按这个规则继承。

schema 与实际执行路由都基于这个集合构建，服务端再验证 `depth <= 1`，不能只隐藏 task 的 schema 却保留可调用的执行入口。父 agent 原有的项目作用域和工具权限仍然生效，继承工具不会扩大父本来没有的权限。

继承的是工具可用性及配置；文件工具需要按子 agent 创建自己的阅读状态，不能直接共享父的全部有状态工具对象。MCP 则复用父可用的共享连接，不重建/关闭连接，取消仍按单次调用处理。

第一版没有强制只读角色。若任务只要求分析，由 prompt 表达不修改文件的要求；这不等于工具层只读限制。worker 的 shell 也不能仅凭 cwd 或 `owned_paths` 实现强制文件写入隔离。

### 5.3 共享工作区的边界

第一版共享主目录，父代理按文件分工，避免两个 worker 同改一个文件。worker 结果回报实际改动与测试，不自动 commit、merge 或回滚子任务改动。

上线 worker 前，把 ToolSharedState 拆为：

- `WorkspaceFileLocks`：按规范化路径索引，至少同一根任务树共享；最好由应用按实际路径共享，覆盖重叠项目目录。
- `AgentReadState`：每个 agent 自己的阅读/版本记录。A 读过文件，不能等同于 B 读过；A 写入后也不能刷新 B 的记录。

只共享整个 ToolSharedState 会破坏 read-before-edit；完全独立又无法串行化跨 agent 的文件修改。锁也管不到 shell、MCP 和外部编辑器，因此仍须保留 stale 检查与明确分工，不把它宣传成完整冲突防护。

worktree 作为后续可选项：先定义 dirty 工作区怎么物化、附加目录怎么处理、结果怎么审阅，再实现创建/保留/清理；不默认自动 merge。

## 6. 运行服务与持久化

建议在 `oct-agent` 内增加 `runtime/` 与 `subagent/` 模块，暂不拆 crate：

```text
API send/resume ──> RunService ──> run_agent_loop
                                     │ task 工具
                                     ▼
                              SubagentManager
                                     │ 子会话 + 恢复 + 监督
                                     ▼
                                RunService ──> 同一个 loop
```

RunService 负责会话占用、模型/工具装配、启动、终态归一与释放。SubagentManager 负责归属关系、子运行启动/恢复、取消、等待和结果查询。先用具体类型与清晰接口，不为尚不存在的远端执行器引入分布式协议。

### 6.1 loop 返回明确结果

把 loop 改成返回 `RunOutcome`，包含终态、最终消息 id/文本、部分输出标记、usage、错误信息。事件继续服务 UI，任务结果不通过监听 SSE 文本拼出来。

统一覆盖正常完成、模型错误、取消和 supervisor 捕获的 panic；进程崩溃后的未结算运行通过持久状态识别。第一版不增加 run 级请求上限或执行期限。最终消息与成功终态应在同一事务落库，不能出现“最终答复已经保存，但重启后仍当作未完成再调用一次模型”的窗口；持久化失败不得仍发布可恢复的成功结果。usage 回填失败可以单独标记未知。请求建立阶段的 `model.stream(request).await` 也必须能响应取消，不能只在 stream 已建立后 select。

活动注册使用占用 guard，错误、panic、future 被 drop 都释放会话占用；异步终态落库由 supervisor 做，不能依赖 Drop 执行 async SQL。等待者消失不等于子任务失去监督，关闭根 run 要等待子清理完成后再释放根的占用。

### 6.2 区分 agent 身份与一次运行

建议最小数据形状：

| 位置 | 新增字段/职责 |
|---|---|
| `conversations` | `kind = chat/subagent`、`parent_conversation_id`、`root_conversation_id`、`agent_type`；子会话仍使用原有 messages/membership |
| `agent_runs`（新表，根/子共用） | `id`、`conversation_id`、`root_run_id`、`parent_run_id`、`parent_tool_call_id`、`status`、`execution_epoch`、有效配置快照、时间、最终消息引用、错误和 usage |
| 内存运行表 | run id、cancel token、join/监督句柄、状态 watch、活动 hub；只存活动对象 |

`agent_id` 首版可等于子 conversation id；新用户任务/后续 followup 分配新的 run id。crash 后 resume 则继续同一个逻辑 run，保留父子和 tool call 关联，仅递增 `execution_epoch` 并创建新内存句柄；旧执行实例的迟到状态/清理不能覆盖新实例。有效配置快照只存模型标识/角色/目录等，不复制 API key、OAuth token。

同一个 `(parent_run_id, parent_tool_call_id)` 只创建一个子 run，避免相同调用重试重复执行；模型新生成的 call id 属于新任务，不凭 prompt 内容做幂等。

终态：`completed | failed | cancelled`；活动态：`running`；可恢复中断态：`lost`。`lost` 表示承载这个 run 的执行实例已丢失，不表示委派已结算。第一版不实现限流排队，不引入 `queued`、`limit_reached`。

重启时将本服务遗留的活动 run 标为 `lost`，保留已持久化历史和父子关联。沿用现有“恢复此轮”的显式入口恢复整棵任务树，流程见 6.4；启动服务本身不自动调用模型。

子会话不进入普通侧栏列表、不自动生成 AI 标题。读消息/原文/状态和取消都验证项目及父子归属；普通 send/resume/compact 路由不能绕过 manager 启动受管子会话。删除父会话/项目先阻止新运行、取消并等待相关运行退出，再事务删除关联记录并复用孤儿内容清理。

### 6.3 执行与取消语义

第一版不做每根/全局子 run 并发上限、容量队列、请求次数预算、run 执行期限或排队等待上限。不加限流 semaphore、排队状态或相应配置；通过现有并行工具执行路径直接启动子任务。每会话只能有一个活动执行实例的互斥仍然保留，它用于防重复启动，不是并发配额。

- 父子循环都保持现有无轮次上限的策略；保留 execute、MCP 等单次工具原有的超时行为，不把工具超时扩大成整个子 run 的执行期限。
- usage 继续统计，缺失标 unknown，不以 0 伪装已知成本。
- 子令牌用父 token 的 `child_token()`。父取消影响运行中的子任务；单独取消一个子任务不取消父和兄弟。对待恢复的 `lost` 任务取消时也要持久化为 `cancelled`，后续 resume 不再重启它。
- 第一版没有 detached 子任务：父 run 取消/失败要清理其子任务；父正常 Finish 前其 task 已全部结算。浏览器断开只是退订，不是取消任务。

### 6.4 crash 后恢复：子任务继续，父任务重建等待

**现有恢复能力**：[`resume_turn`](../../crates/oct-agent/src/api/chat.rs) 在用户点击“恢复此轮”后，检查悬空 ToolCall 或末条未回答的 user 消息，解析原模型，调用 [`repair_dangling_tool_calls`](../../crates/oct-agent/src/db/messages.rs) 补入“工具执行被中断（后端重启），工作区状态未知”的错误 ToolResult，再加载历史启动同一个 loop，不新增用户消息。普通工具不会被运行时直接重放，模型看到中断信息后决定如何继续。这是从持久化历史续跑，不是恢复原进程的 future 或半截输出。

subagent 复用这套修复和续跑机制，父级 `task` 增加一层恢复协调：

1. 用户在主会话点击一次“恢复此轮”。RunService 取得根会话占用并准备模型/工具，manager 根据持久关联找到该轮尚未结算的 task，恢复父子执行实例及取消关系。恢复请求并发到达时不能重复启动子任务。
2. 对已 `completed` 的子 run，直接读取已保存的最终结果，不再调用子模型；若该结果尚未写到父历史，则补回原 `parent_tool_call_id`。`failed/cancelled` 同样回传已有终态。
3. 对 `lost` 的子 run，在原子会话上修复其普通悬空工具结果，然后从原历史继续运行；同轮未完成的兄弟可以一起恢复。子已在运行时只接回等待，不重复启动。模型/凭据解析等准备失败时保留可恢复状态并向 UI 显示错误，释放本次恢复的占用，允许修正配置后重试；整批恢复先完成准备再启动，避免部分启动后根占用被释放。
4. 父级未结算的 task 不走普通“补中断错误”修复，而是重新建立等待。等待发生在宿主里，父模型不需要靠重复请求或轮询来等子模型。子完成后，将结果写入原 task 对应的 ToolResult。
5. 父同轮中的普通悬空工具仍按现有规则修复；所有 task 结算且整批 tool-call/result 配对闭合后，再构造父的 LlmView 并继续父 loop。这样用户看到的就是“子 agent 继续，主 agent 等完后继续”。

崩溃会同时丢失父子内存句柄，所以“主 agent 继续等”需要从数据库重建等待关系，不能仅重新调用子会话的 HTTP resume。共用 RunService 内部恢复逻辑，避免子代理工具调用 REST handler。

实现时必须覆盖这些边界：

- **恢复资格看 run 状态**：现有“悬空 ToolCall 或末条 user”的判断不够。crash 可能发生在所有工具结果已落库、下一次模型请求尚未完成时，此时末条是 tool；`lost` 根/子 run 仍必须可恢复。前端恢复提示也读取持久 run 状态。
- **先识别 task，再修复普通工具**：send/resume 共用的修复入口需能区分受管 task，不能把正在等待恢复的 task 提前补成错误结果。存在待恢复委派时，普通 send 返回明确的待恢复状态，UI 引导先恢复此轮或停止旧任务；不能绕过协调器插入新用户消息。
- **注册边界**：子会话、首条委派消息、子 run 与父 call 的关联在同一事务创建，再启动执行；若 crash 时父 task 已落库但关联尚未创建，恢复时按原 call 参数幂等补建；已有关系则复用。
- **结果只落一次**：按父 run + tool call id 在事务中检查/写入结果。覆盖“子已完成、父未收到”“父已收到、尚未继续采样”和恢复过程再次 crash；最终消息与子成功状态原子落库，避免重复子执行或重复 ToolResult。
- **恢复不等于重放副作用**：普通 shell/MCP 悬空调用仍补中断信息，不因 resume 自动再执行原调用；已存在结果的调用保留。模型基于历史可能重新选择调用工具，这与现有主 agent 恢复语义相同。

第一版只保留一个树级恢复入口。子 panel 的 lost 提示提供“返回主对话恢复”，避免逐个恢复子任务却遗漏父级等待关系；停止整个树时将未完成子任务一并终结。

## 7. UI、SSE 与使用量

### 7.1 主 agent 卡片 → 完整子 agent panel

父消息里新增 TaskCard，显示任务描述、worker 标记、运行/待恢复/终态、耗时与结果摘要。卡片内始终提供「打开子代理」入口，运行中即可点击，不必等 task 返回。参考 [`redesign-demo.html`](../../redesign-demo.html) 的 `.step.subagent`、`.sub-open` 与 `openTab(..., 'sub')`：在工作区打开并激活一个 `子代理 · <任务描述>` 标签页，其内容是和主 agent 一样的完整聊天 panel。

- 复用 [`ChatPanel`](../../packages/app/src/components/chat/ChatPanel.vue) 的面板结构，以及 ChatMessage、思考块、工具卡片、执行输出、diff、压缩分隔条等展示组件；不是只显示最终摘要或另做纯文本日志窗口。
- 展示委派消息和子 agent 的完整处理过程：正在输出的文本/思考、调用的工具及参数、实时终端输出、工具结果、最终答复；沿用主 panel 的滚动和折叠体验。
- 标签身份按 `agent_id` 去重，运行数据按 `run_id` 区分；相同标题的不同子 agent 不能误合并，重复打开已有 agent 则激活原标签。后续 followup 可以继续复用同一 agent 标签。
- panel 顶部标识子代理、任务状态与父会话，提供「返回主对话」；保留主 panel 的历史、滚动位置和输入草稿。关闭子标签只关闭视图，不取消任务；可单独点击停止来取消该子 run。
- 第一版 panel 是受管会话的实时查看模式：复用完整消息展示，但不提供独立发送、模型切换或直接 resume 入口；输入区用任务状态、子上下文用量和停止操作替代。运行中通信和续派由第 8 节单独定义，避免复用主 panel 时顺带绕过 manager。

卡片内可以保留紧凑预览，但完整 panel 是第一版必要功能，不以预览替代。通过 [`WorkspaceTabBar`](../../packages/app/src/components/layout/WorkspaceTabBar.vue) 与标签状态接入，使用 reka-ui 的现有交互原语。

### 7.2 底部 subagent 指示器

在 [`BottomBar`](../../packages/app/src/components/BottomBar.vue) 增加当前根会话的子代理指示器，参考 demo 的 `#stTasks`、`#bgTaskBtn` 与向上展开的 `#bgDD`：紧凑图标/状态点 + 数量，位置对齐聊天面板左缘，随布局尺寸调整。demo 的底部示例内容实际是 `pnpm dev` 后台进程；这里沿用其位置和交互，列表内容改为 subagent，不引入后台 shell 管理功能。

- 根会话没有子任务时隐藏；有任务时显示「子代理 N」，N 为 `running` 数量。运行时有动画；存在 `lost` 时显示“待恢复”标记，不能将它误显示为已完成或仍在执行。全部结算后显示完成或异常状态并保留入口，数量可为 0，历史结果仍可打开。
- 点击后向上展开列表，展示该根会话的子任务：任务描述、状态、耗时及简短活动信息（如正在读文件、执行命令）；活动任务优先，已结束任务按时间排列，历史分页加载。
- 点击某一项与 TaskCard 使用同一个 `openSubagentPanel(agent_id)` 动作；运行中和已完成任务均可打开。运行项提供独立停止操作，点击停止不同时触发打开面板。
- 从主标签切到某个子标签时，指示器仍显示所属根会话的子任务集合，便于在兄弟任务间切换；切到另一根会话时切换集合，不混入其他会话/项目的任务。非聊天标签沿用当前选中的根会话，无根会话上下文时隐藏。
- 弹层用 reka-ui Popover/DropdownMenu，支持键盘打开、选择与 Esc 关闭。定位与碰撞处理交给组件；布局位置遵循 demo，不复制 demo 的手写浮层管理。

底部指示器和完整 panel 都在第一版交付，与 `task` 是否等待结果无关：即使父模型正在等待，用户仍然能实时观察所有 worker。

### 7.3 父子流、状态与使用量隔离

父 hub 仅发布 `subagent_updated` 一类小事件，带 parent call id、agent id、run id、execution_epoch、状态与必要摘要；供卡片和底部列表展示的活动摘要应有界、节流。子代理完整 text/reasoning/tool 事件留在子 hub，打开对应 panel 时按需订阅。子 `Finish`、`Usage` 不可直接作为父事件转发，否则会错误结束父流或覆盖父上下文环。

父 hub 重连重放只提供活动 run 信息；结束后通过 DB/子任务列表重建卡片和底部列表。以 run id 做任务身份去重，以 execution_epoch 隔离恢复前后的执行事件；历史卡片即使被 microcompact 清除了文本，仍可沿父调用关联查到子任务。第一版不引入 JSONL 或全局事件序号，沿用已有全 run 内存重放策略。

父子 panel 复用同一套历史转换、SSE reader/reducer 与渲染组件，但各自持有实例化的消息、流、usage 与滚动状态，带 conversation id + generation 检查；不能让子 panel 改写现有 `useChat` 的模块级当前会话。关闭子 panel 释放它的完整消息订阅，保留后端执行。新运行不能复用旧的终态 watch；任务管理器、转发任务都必须及时释放终态 hub，保障 SSE EOF。

根会话的子任务状态 store 独立于当前可见 panel：切到子标签或关闭底部弹层后，仍通过根 run 的状态事件更新任务列表，不需要常驻订阅所有子任务的完整输出。初次进入、切换根会话及重连时拉取状态快照并与当前 run 的事件对账，旧根会话迟到响应不能污染新根会话。任务终态持久化先于状态发布；根流 EOF 后再用快照收敛。

上下文占用环只计算各自会话的窗口，模型标识展示子任务实际使用的模型；总费用/usage 单独聚合根及子 run 的实际请求，包含压缩请求，避免把子 usage 当成父 context anchor，也避免工具返回的汇总被重复记账。

## 8. 后台与通信的演进条件

第一版明确接受：父模型要等整批 task 完成才能继续。确认这个等待成为瓶颈后，再对同一 manager 增加：

1. `task(background=true)`：注册后立即返回 agent/run id。增加 `get_task_output`（快照、可选阻塞等待、分页全文）和 `cancel_task`。wait 超时只返回 running，使用 watch/Notify，禁止 sleep 轮询。
2. 完成结果可靠投递：在同一事务写子终态与父 inbox 记录，以 `(child_run_id, event_kind)` 去重。父在完整工具结果批次后的下一次采样边界，把 inbox 转成带来源标记的上下文消息，并与消费标记同事务提交；不能直接插入尚未闭合的 tool-call/result 之间。同步 task 已返回结果的路径不重复投递。
3. 默认结构化存活：父准备 Finish 时若仍有未结束或未交付子任务，由 supervisor 等待结算、回灌结果再让父收尾；这是宿主规则，不能仅依赖提示词要求模型记得 wait。用户取消整个 run 时全部停止。暂不允许父已结束后靠子任务自动启动新父 run。
4. 最后加入明确分离的 `send_message` 与 `followup_task`：前者只排队、不唤醒 idle；后者可启动终态子会话的新 run，并返回新的 run id。忙碌时先拒绝 followup 以避免模糊排队；运行中消息只在完整工具批次后的安全点生效，不打断流的一半。

只有需要「父结束后子仍继续」时，才增加 detached 生命周期、持久通知、空闲会话唤醒与并发用户输入仲裁。那是一个独立产品能力，不是 background 参数顺带带来的行为。

自定义 agent 文件、按任务换模型、选择性 fork、worktree、递归、兄弟通信、消息板、强 schema 产出分别按需求加；不把它们绑成一次平台重构。

## 9. 建议的开发拆分与验收

### PR 1：统一 run 生命周期

- 抽 RunService、会话占用 guard、RunOutcome；补 run id 与持久运行状态，根 run 先接入。
- 复用 backlog 已提出的聊天应用服务重构，保持 send/resume 各自 repair/写入顺序。
- 验收：同会话并发仍 409；模型错误、准备失败、取消、panic 后可再次启动；SSE 正常 EOF；已有压缩、恢复、工具测试不退化。

### PR 2：worker 执行与写入协作

- 子会话关系、SubagentManager、task/worker、并行执行/取消、父子任务树恢复；task 仅接收 description/prompt。
- worker 继承父当前全部工具，只排除 task；拆分共享文件锁与 agent 阅读记录，继承可用 MCP 连接，补 worker 提示词。
- 提供卡片、panel 和底部列表需要的状态/历史/事件/取消 API 与 OpenAPI 契约；初版不含后台开关。
- 验收：父一次发两个 task，二者确实同时开始；子无父历史，工具 schema 与执行路由均精确等于父集合减 task；写工具、shell、MCP 可正常执行；结果只回父一次；单个子失败不影响兄弟；父取消后无遗留活动子 run。
- 验收写入：A 的 read 不授权 B 的 edit；B 在 A 修改后须重新 read；受工具锁管理的同路径修改串行；取消子 MCP 调用不关闭共享连接；跨任务查询/取消被拒绝。
- 验收恢复：模拟重启后根/子均为 lost，一次恢复操作重启未完成子任务并重建父等待；子已完成直接取结果；子工具结果已齐但末条为 tool 时仍能恢复；父在所有子结果配对前不请求模型。恢复并发、恢复中再次 crash、子已完成父未接收均不重复启动或写结果；普通工具不被运行时重放。
- 说明 shell/MCP 仍可能绕过文件工具锁，用分工降低冲突；不承诺自动合并。

### PR 3：TaskCard、完整子 panel 与底部指示器

- 将聊天展示与会话状态实例化，父子共用 ChatPanel/ChatMessage 的展示链路；接入 subagent 标签、TaskCard 打开入口与返回父会话。
- 按 demo 的底部任务入口实现 subagent 指示器、状态列表及打开/停止动作；卡片和底部入口复用同一个 panel 打开逻辑。
- 验收：task 尚未完成即可从卡片打开子 panel，看到持续的文本/思考、工具调用、执行输出和 diff；完成后仍可看完整历史；重复点击只激活已有标签，相同标题不同 agent 各有标签。
- 验收隔离：切换父子标签保留主会话草稿/历史位置；子 Finish 不结束父流，子 usage 不污染父用量；关闭子标签不取消运行；旧订阅事件不串到新面板。
- 验收底部入口：多个 worker 的运行计数准确，结束/失败/取消后即时收敛；crash 后显示待恢复，恢复后回到运行状态；子标签内可切换兄弟任务；换根会话不串列表；刷新/重连能重建卡片及指示器；键盘可打开列表并进入 panel。

### 后续 PR：后台任务，再到通信/自定义角色

- 仅在需要父子同时推进时实施第 8 节；先可靠结果与父收尾屏障，后消息和 followup。
- 验收：wait 超时不杀任务；子完成与父 Finish 竞争时不丢结果；重复完成通知不重复注入；进程重启不自动重放副作用；取消、followup 与旧运行迟到结果按 run id + execution_epoch 隔离。

测试沿用 [`tests/support`](../../crates/oct-agent/tests/support/mod.rs) 的 ScriptedModel、真实 SQLite 与 HTTP mock，并增加可按 agent/run 分配的模型工厂，避免父子并发抢同一剧本队列。并发用 gate/Notify/CancellationToken，不靠 sleep，不调用真实模型。

前端覆盖父子流隔离、重连/历史一致性、面板开关不取消、旧订阅迟到不污染新视图。实现时运行相关 Rust 集成测试、前端单测/浏览器测试与 build；改 SQL 必须迁移并更新 `.sqlx`，改事件/DTO 必须再生成 schema。
