# packages/app AGENTS.md

前端：Vue 3 + Vite 8 + UnoCSS (OKLCH tokens)。`openapi-fetch` 消费生成的 OpenAPI schema，`@vueuse/core` composables，`unplugin-vue-components` 自动导入。Assistant 消息渲染用 `vue-stream-markdown`（流式 markdown），代码高亮 `shiki`（`utils/highlight.ts`）。

命令从仓库根执行（见根 `AGENTS.md`）：`pnpm --filter frontend dev|build|generate-api|test|test:browser`。

- **Path alias**: `@` → `packages/app/src`（Vite resolve alias）
- `build` 先跑 vue-tsc typecheck；`generate-api` 从运行中的服务器重新生成 `src/api/schema.d.ts`
- **UI primitives**: 交互组件（dialog、popover、dropdown、splitter 等）优先使用 [reka-ui](https://reka-ui.com/)，不要手写浮层/焦点管理。文档入口：<https://reka-ui.com/llms.txt>。已在用：`App.vue` 的 Splitter、`SettingsModal.vue` 的 Dialog、`BottomBar.vue` 的 DropdownMenu（含 Sub）、`WorkspaceTabBar.vue` 的 DropdownMenu。

## 目录

- `components/views/TerminalTabView.vue` + `composables/useTerminal.ts`：Ghostty Web 终端懒加载 WASM，标签切换用 v-show 保活，关闭标签经 REST 删除 PTY，视图卸载只断 WebSocket；刷新按项目重新发现会话，未激活的恢复标签不抢连接。用户输入在 live 帧后启用；每帧渲染及同步查询回复发送完后 ack 确认字节位置，重放按 ready.replay_reply_after 切分（即使边界在帧内），仅抑制已处理过的旧查询，断线期间新查询照常回复。按原始字节计数重连，不重发用户输入。Ghostty 精确钉 next 版本以包含上游 DA 回复修复，升级需保留浏览器协商测试。
- `src/composables/`: `useChat.ts`（公开 facade/主面板单例；主面板与子代理各有独立 session，子面板 `contextUsage:false`）, `useSubagents.ts`（根会话任务 store:快照+`subagent_updated` upsert/epoch 去重,TaskCard 与底部指示器共用）, `useProjects.ts`, `useProviders.ts`, `useSettings.ts`（含 wire 日志开关）, `useAgentSettings.ts`（worker/title 配置）, `useMcpServers.ts` + `useMcpServerForm.ts` + `useMcpStatus.ts` + `useMcpPromptComposer.ts`, `useSlashCommands.ts`（斜杠命令菜单）, `useAttachments.ts`（聊天附件上传队列:入队即传(XHR 进度+并发2)/chip 状态机/发送门禁=全部 ready；fork/revert 草稿签发目标会话的新 staged ticket（共享资产字节），失败可重试、切会话取消；UI 在 `ChatPanel` 拖拽 + `AttachmentChips`）, `useBackgroundTasks.ts`（后台 bash 任务快照 store:列表轮询(存在非终态且页面可见时 2s)+cancel upsert+唤醒 run 宽限窗探测）, `useContextUsage.ts`（上下文占用指示器）, `useGit.ts`（Changes 面板：status/diff/history/stashes/actions）, `useFileTree.ts` + `useFileContent.ts`（文件树/文件预览）, `useConversationFork.ts` / `useConversationRevert.ts`, `useTabs.ts`（工作区标签页）
- `src/components/`: `chat/`（含 BashTaskCard 后台任务卡/TaskNotificationDivider 通知分隔条/ApprovalBanner/输出查看 Dialog）, `subagent/`（TaskCard/子代理面板）, `changes/`（Git Changes）, `diff/`, `file-tree/`, `views/`（标签页视图）, `layout/`, `ui/`, `settings/`（含 `agents`/`mcp`/`permissions`/`provider` 子面板）, `sidebar/`
- `src/composables/chat/`：`session.ts` 编排发送/恢复/根子挂流/收尾；`timeline.ts` 独占展示写入、原始快照、乐观消息、50ms buffer、历史版本与回放交接；`stream.ts` 是无 Vue/IO/时钟依赖的确定性 reducer；`submission.ts` 按本次请求回执确认提交，不自动重发 POST；`approvals.ts` 的快照带请求序号和事件版本守卫；`usage.ts` 是实例用量，只有 facade 注入全局同步；`api.ts` 请求/响应归一化。`run.ts` 区分 view generation、操作 owner 与 history-change 互斥，旧事件/旧 finally 不得更新新视图；历史变更结束后的刷新必须在首个 await 前捕获 view lease，历史刷新与后续 run 状态查询各自 await 后都校验同一 lease，A→B→A 也视为旧视图；`transport.ts` 按序等待异步元数据初始化再交付内容。
- 重放使用 `run_meta` 的 `run_id + execution_epoch + history_epoch + replay_after_revision`：先按原始 DTO 的 `appended_at_revision` 选持久化前缀，再转换展示并从空 overlay 重放。禁止按最后 user 或可重排的 ordering 猜边界；主/子 `/messages` 都带 revision/epoch headers。缺少/不兼容边界保留历史并结束该订阅；子代理退订不取消后端。
- `src/composables/chat/attachments.ts` 是发送/草稿的共享附件类型；历史展示优先用 `MessageDto.user_content.body/attachments`，不显示模型用的固定文本快照。旧历史 fallback 集中在 `chat/display.ts`。
- `src/api/`: `client.ts` (openapi-fetch), `schema.d.ts` (auto-generated)

## 字号

- 字号与行高集中在根 `uno.config.ts` 的 `fontSize`，同时生成 `--oct-font-size-*` / `--oct-line-height-*` CSS 变量。组件使用 token 类，避免新增 `text-[…px]`。
- `text-base` 14px / 22px：默认正文、主界面文字；`text-sm` 13px / 20px：紧凑控件；`text-xs` 12px / 18px：辅助信息；`text-badge` 11px / 16px：徽标。
- `text-lg` 16px / 24px、`text-xl` 18px / 28px：两级标题。对话 Markdown 的 h1/h2 为 18/16px，h3–h6 与正文同为 14px。
- `text-code` 12px / 20px：行内代码、代码块、文件预览、diff 与工具输出；终端读取 `--oct-font-size-terminal`（13px），不在 JS 中重复写字号。
- `MarkdownText.vue` 将库内字号变量映射到上述 token，并覆盖代码行与行号的显式字号；修改时同时核对表格、行内代码和代码块。

## 单元测试

- vitest 测试与源码同目录（`src/**/*.test.ts`）。独立 `vitest.config.ts`，node 环境，不加载 vue/unocss 插件。
- 测 composable 时优先经公开 API 驱动：mock `@/api/client`（REST）与全局 `fetch`（SSE），不导出私有函数。
- `useChat` 的 SSE mock：用 `ReadableStream` 构造 `Response`，可按任意字符串切块模拟网络分片。
- 发送失败（含 HTTP 错误）不等于消息未提交：按 `client_request_id` 回执对账后才清空 composer 的正文/附件。回执探测仅在确认时替换历史，未确认不得清掉已有快照与 revision；run 装配失败但消息已落库时走 resume，不重发已认领附件。历史草稿恢复按 `draft_attachments` 复制附件，正文不重复内联文件内容；恢复后的服务端缩略图 URL 不走 objectURL revoke。

## 浏览器 e2e（拦截层）

- `browser-e2e/` + 独立 `vitest.browser.config.ts`：vitest browser mode（Playwright chromium）在真实浏览器里挂载完整 App（复用 `main.ts` 的装配 + vue/unocss/unplugin 插件链），黑盒驱动 UI 全流程。
- "后端"是剧本化 mock 服务器（`browser-e2e/mock/server.ts`，node:http 零依赖）：配置求值时启动（随机端口 + `unref`），经 Vite dev server 的 `/api` 代理注入——与 dev/preview 同一条代理路径，浏览器走真实 fetch → HTTP → 代理 → SSE。
- 剧本按会话组织（`scripts[convId]`）。步骤：`events`（AgentEvent 数组；`slices`/`cut` 做字节级切块，边界自然落在多字节 UTF-8、`data:` 行、JSON 中间）、`comment`（keep-alive 注释行）、`gate`（命名门：测试经 `/__mock/gates/:name/open` 放行，是流中暂停的同步原语，禁 sleep）、`status`（POST 直接回错误码）、`hang`（挂流，配合 cancel 注入 `cancelled`）。
- 管理 API 同源（`/__mock` 代理）：`reset` / `scenario` / `gates/:name/open` / `requests`（请求日志，断言 cancel 调用、`provider_spec` 等）。
- 约定：fixture 显式对齐 `schema.d.ts` 类型（后端改字段 → 前端这边编译期报错）；断言一律 `expect.poll(chatText)`（`useChat` 每 50ms flush 一次 stream buffer，瞬时断言必 flaky）；**测试文件必须串行**（`fileParallelism: false`，mock 状态是全局单例，并发会互相覆盖剧本）；每个文件独享 iframe（composable 模块级状态按文件隔离）。
- 不启真后端、零真实模型，CI 铁律天然满足。

## Lint

`pnpm -r exec eslint .`（@xwbx/eslint-config flat config，从仓库根跑）。

聊天边界验收以 `useChat` 公开 API 驱动（包含 A→B→A、旧 finally、历史变更收尾迟到、发送回执不确定、审批快照竞态、主/子实例隔离）；`chat/transport.test.ts` 确认异步 run_meta 初始化先于内容消费。`timeline.messages` 只读对外，session 不自行裁剪历史或匹配工具卡；reducer 不导入 Vue/网络/全局 store，submission 不修改 timeline/composer，底层模块不反向导入 useChat。
