# packages/app AGENTS.md

前端：Vue 3 + Vite 8 + UnoCSS (OKLCH tokens)。`openapi-fetch` 消费生成的 OpenAPI schema，`@vueuse/core` composables，`unplugin-vue-components` 自动导入。Assistant 消息渲染用 `vue-stream-markdown`（流式 markdown），代码高亮 `shiki`（`utils/highlight.ts`）。

命令从仓库根执行（见根 `AGENTS.md`）：`pnpm --filter frontend dev|build|generate-api|test|test:browser`。

- **Path alias**: `@` → `packages/app/src`（Vite resolve alias）
- `build` 先跑 vue-tsc typecheck；`generate-api` 从运行中的服务器重新生成 `src/api/schema.d.ts`
- **UI primitives**: 交互组件（dialog、popover、dropdown、splitter 等）优先使用 [reka-ui](https://reka-ui.com/)，不要手写浮层/焦点管理。文档入口：<https://reka-ui.com/llms.txt>。已在用：`App.vue` 的 Splitter、`SettingsModal.vue` 的 Dialog、`BottomBar.vue` 的 DropdownMenu（含 Sub）、`WorkspaceTabBar.vue` 的 DropdownMenu。

## 目录

- `src/composables/`: `useChat.ts`（`createChatSession()` 工厂——主面板单例 `useChat()` 之外,子代理面板持有实例化会话状态:`attachSubagent`/`dispose`/`contextUsage:false`；`chat/run.ts` 统一 conversationId + generation 所有权与 begin/finalize，切会话主动 abort 视图订阅，旧事件/旧 finally 不得写消息/usage/busy；审批快照带请求序号与事件版本守卫）, `useSubagents.ts`（根会话任务 store:快照+`subagent_updated` upsert/epoch 去重,TaskCard 与底部指示器共用）, `useProjects.ts`, `useProviders.ts`, `useSettings.ts`（含 wire 日志开关）, `useAgentSettings.ts`（worker/title 配置）, `useMcpServers.ts` + `useMcpServerForm.ts` + `useMcpStatus.ts` + `useMcpPromptComposer.ts`, `useSlashCommands.ts`（斜杠命令菜单）, `useAttachments.ts`（聊天附件上传队列:入队即传(XHR 进度+并发2)/chip 状态机/发送门禁=全部 ready；fork/revert 草稿签发目标会话的新 staged ticket（共享资产字节），失败可重试、切会话取消；UI 在 `ChatPanel` 拖拽 + `AttachmentChips`）, `useBackgroundTasks.ts`（后台 bash 任务快照 store:列表轮询(存在非终态且页面可见时 2s)+cancel upsert+唤醒 run 宽限窗探测）, `useContextUsage.ts`（上下文占用指示器）, `useGit.ts`（Changes 面板：status/diff/history/stashes/actions）, `useFileTree.ts` + `useFileContent.ts`（文件树/文件预览）, `useConversationFork.ts` / `useConversationRevert.ts`, `useTabs.ts`（工作区标签页）
- `src/components/`: `chat/`（含 BashTaskCard 后台任务卡/TaskNotificationDivider 通知分隔条/ApprovalBanner/输出查看 Dialog）, `subagent/`（TaskCard/子代理面板）, `changes/`（Git Changes）, `diff/`, `file-tree/`, `views/`（标签页视图）, `layout/`, `ui/`, `settings/`（含 `agents`/`mcp`/`permissions`/`provider` 子面板）, `sidebar/`
- `src/composables/chat/attachments.ts` 是发送/草稿的共享附件类型；历史展示优先用 `MessageDto.user_content.body/attachments`，不显示模型用的固定文本快照。旧历史 fallback 集中在 `chat/display.ts`。
- `src/api/`: `client.ts` (openapi-fetch), `schema.d.ts` (auto-generated)

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
