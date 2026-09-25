# 后台 Bash 运行时设计：envelope 即答、文件直写输出、通知唤醒

- 状态：已实施（第一版）
- 日期：2026-09-25
- 依据：[七仓后台执行调研](../explore/ai-agent-bash-background.md)与当前 OCT 源码（ADR 0002 之后的代码形态）。

## 1. 决策

`execute_command` 增加 `run_in_background` 参数，把长命令交给独立的**后台任务运行时**：

1. **envelope 即答**：工具调用立即返回携带 `task_id` 的信封，tool call 在历史里是合法闭环——不产生悬空调用，与 dangling 修复、`pending_recovery` 车闸、microcompact COW 全部零交互。
2. **输出文件直写**：子进程 stdout/stderr 直接绑到输出文件 fd（`O_APPEND`、0600），字节不经过后端进程；文件在**工作区之外**（`{db 目录}/exec/{conversation_id}/{task_id}.log`，`OCT_EXEC_DIR` 可覆盖），沙箱化的 `read` 工具够不到它，模型读输出只能走 `task_output` 工具——不破沙箱。
3. **通知唤醒而非轮询**：任务终态后，per-conversation 投递循环等会话空闲（occupy 串行天然合批），把所有待通知任务合成为**一条** `<task-notification>` user 消息（内联输出尾部 4KiB），与 `notified` 标记同事务插入，然后以会话上次 provider/model 启动新的 root run——模型被主动叫回来处理结果。信封文案明说 "You will be notified automatically — do not poll or sleep"。
4. **随后端死、不随 run 死**：子进程沿用前台路径的进程组 + Linux PDEATHSIG（后端退出即被内核终结，下次启动 bootstrap 把 running 行标 `lost`），但**不进**工具 future 的 `kill_on_drop`/组杀守卫作用域——run 取消/结束不杀已信封化的任务（信封承诺了通知，兑现它）；只有 `task_stop`、HTTP cancel、会话删除才杀。杀法是两阶段：组 SIGTERM → 1.5s 宽限 → 组 SIGKILL → 有界 reap。
5. **内存注册表 + DB 真相**：内存只存活动任务（token、done watch、输出路径、命令）；`background_tasks` 表是查询、重启清扫与通知补投的单一事实源，终态写入带 `status='running'` 一次性守卫。结构镜像 SubagentManager（`OnceLock<Arc<RunService>>` 两阶段接线）。

明确不做（第一版）：前台超时自动转后台（后续 PR，需前台输出文件化）；交互式 stdin；子代理后台（见 §4）；任务列表/批量等待工具。

## 2. 模型可见接口

```json
{"name": "execute_command", "arguments": {
  "command": "cargo build --release",
  "run_in_background": true}}
```

信封回复：task_id + 「用 task_output 查状态/读输出、task_stop 停止、完成会自动通知」。

| 工具 | 入参 | 语义 |
|---|---|---|
| `task_output` | `task_id`；`block?`（默认 false）；`timeout_secs?`≤600；`offset?`（字节，缺省读尾部）；`max_bytes?`≤1MiB 默认 32KiB | 状态行 + 有界输出切片；分页带 `next_offset` 提示；NotFound 附本会话已知 id（模型可自我纠正） |
| `task_stop` | `task_id` | 取消 + 有界等结算；已终态报 already exited；lost 行持久化为 cancelled |

REST 面（仅前端）：`GET /conversations/{id}/background-tasks`（running 优先）、`GET /background-tasks/{id}/output?offset&limit`、`POST /background-tasks/{id}/cancel`。

## 3. 生命周期

```
execute(run_in_background)
  → 校验 cwd（WorkspaceRoots）
  → spawn sh -c：进程组 + PDEATHSIG、stdio=输出文件 fd、无 kill_on_drop
  → INSERT background_tasks(running)（id 调用方生成：注册表键=文件名=行 id 三位一体）
  → 注册内存条目（token + done watch）→ spawn supervisor → 返回信封

supervisor: select { child.wait | cancel.token | 24h 上限 | 5s watchdog stat 文件 ≥1GiB }
  → 自然退出=completed（非零退出码不算失败，通知里带码）；
    cancel=cancelled；watchdog/24h=failed（error 注明原因）
  → finish_task（一次性守卫）→ done_tx.send(true) → 移除内存条目 → kick 投递循环

投递循环（kick-once 标记防重入）:
  loop { 查 notified=0 且非 running 的行 → 空=退出；否则 deliver }
  deliver: occupy（busy 则 1s 重试）→ 重查 pending（等待期间新结算的并入本批）
    → 单事务：插 user 通知消息 + 置 notified=1
    → get_last_provider_spec 解析模型（同 resume 规则；失败只丢 run 不丢消息）
    → load_llm_view（含通知）→ set_start_message_id → run_root_with_guard（不触发 title 生成）

兜底: bootstrap 把 running 标 lost；send/resume 入口 catch_up_notifications
  把未通知终态（含 lost，文案注明「被后端重启中断」）作为前置消息补投。
```

会话删除：先 `cancel_tasks_for_conversation`（有界等结算）再删（行随 FK 级联、输出目录随删）。envelope 即答意味着**没有**悬空 call，恢复资格判定（dangling/末条 user/lost+tool 尾）不受影响。

## 4. 边界与取舍

- **子代理禁用后台**：`derive_child_toolset` 重建 `execute_command`（schema 完全一致、gate 关闭）并丢弃 `task_output`/`task_stop`。理由：唤醒语义只在 root 会话成立（子会话是 managed，send 409），子代理的后台输出无处通知。子代理里传 `run_in_background:true` 得到明确错误文案。
- **非零退出码是结果不是失败**（七仓共识）：`completed` + 通知带 exit_code；`failed` 留给机制性击杀（输出超限/24h）。
- **通知幂等**：消息插入与 `notified` 置位同事务；投递循环经 occupy 串行，同批多个完成只产生一条消息、一个模型轮（避免 turn 风暴）。
- **唤醒 run 失败的降级**：消息已入库即算送达；模型解析/启动失败只记日志，下次 send 时模型自然看到通知。
- **运行取消 vs 任务取消**解耦是承诺语义的要求：信封说了「会通知」，run 被取消后任务照跑、通知照发（投递循环等空闲）。
- **已知缺口**（记录，不在本版修）：崩溃恢复重建的子代理工具集本就缺 execute/grep/glob/listdir（`start_child_run` 的 `parent_toolset=None` 分支，先于本 ADR 存在）；项目删除经 SQL 级联删会话时不会先取消后台进程（会话删除入口做了，项目入口未做）。

## 5. 测试

`tests/background_tasks.rs`（7 例，全部真实 `sh -c` 进程 + 剧本化 LlmMock，零真实模型）：

1. 全链路：信封即答 → 运行中输出文件可读（fd 直写证据）→ 结算一次 → 通知唤醒（断言 mock 收到的请求含 `<task-notification>`/exit_code/尾部）→ REST 列表/历史；
2. run 取消不杀后台任务（marker 文件 + 通知照发）；
3. HTTP cancel 结算 cancelled + cancelled 通知 + 未知 id 404;
4. 输出 watchdog（上限缩到 256B）击杀为 failed;
5. 崩溃模拟（WAL checkpoint + db 复制 + `new_with_dir`）：bootstrap 标 lost，下次 send 的 catch-up 把 lost 通知插在用户消息之前（同一请求可见两者）；
6. 会话删除级联（行消失 + 输出目录清理）；
7. 工具级直调：task_output 非阻塞/阻塞/分页、task_stop 终态与活任务、子代理 gate、后台实例信封。

同步原语：唤醒 run 无测试侧订阅，用「轮询 LlmMock 请求日志 + 30s 截止」的有界等待（等价于前端 e2e 的 `expect.poll`）；其余等 SSE EOF。

## 6. 后续方向

- 前台超时自动转后台（前台输出 tee 落盘化后原地翻转）；
- `BackgroundTaskUpdated` hub 小事件（run 活跃期间 UI 实时翻转，替代前端轮询）;
- 前端（PR2）：信封卡片转后台任务卡（状态/耗时/字节/停止/查看输出）、通知消息渲染为全宽分隔条、底部指示器、唤醒 run 的 attach 流式;
- 子代理后台策略（如限额）与列表/批量等待工具按需评估。
