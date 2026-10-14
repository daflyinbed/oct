# frontend（packages/app）

oct 的前端：Vue 3 + TypeScript + Vite 8 + UnoCSS（OKLCH design tokens）。开发时独立 dev server，发布时构建产物烧入 `oct-agent` 二进制（rust-embed，history 路由由 SPA fallback 兜底）。

## 界面能力

- 多项目/多会话工作区：侧栏、标签页、文件树与文件预览、Git Changes 面板
- 聊天：SSE 流式渲染（`vue-stream-markdown` + shiki 高亮）、thinking/tool 卡片、斜杠命令（内置命令 + MCP prompts）、上下文占用圆环、`/compact`
- 子代理任务卡、后台 bash 任务卡（输出查看/停止）、审批横幅（allow/allow_always/deny）
- 对话 fork / revert（含切点选择与历史回滚对话框）
- 设置：providers/models、MCP servers（连接状态/OAuth 登录）、Agents（worker/title）、权限规则、wire 日志与工具调用方式开关

## 技术栈

| 关注点 | 选择 |
| --- | --- |
| API 客户端 | `openapi-fetch` + 生成的 `src/api/schema.d.ts`（类型安全，后端 OpenAPI 是唯一事实源） |
| Composables | `@vueuse/core` + 本地 `src/composables/` |
| 组件自动导入 | `unplugin-vue-components` / `unplugin-icons` |
| UI 原语 | reka-ui（dialog、popover、dropdown、splitter…） |
| 样式 | UnoCSS |

## 命令（从仓库根执行）

```bash
pnpm --filter frontend dev            # Vite dev server（5173，/api 代理到后端 3000）
pnpm --filter frontend build          # vue-tsc typecheck + vite build
pnpm --filter frontend generate-api   # 从运行中的服务器重新生成 src/api/schema.d.ts
pnpm --filter frontend test           # vitest 单测（node 环境，与源码同目录）
pnpm --filter frontend test:browser   # 浏览器 e2e 拦截层（首次需 playwright install chromium）
```

## 开发要点

- 后端改了 API（路由/DTO）→ 启动后端后跑 `generate-api`，`schema.d.ts` 变更随 PR 提交；fixture 显式对齐该类型，字段不匹配会编译期报错。
- 发布顺序：先 `pnpm --filter frontend build` 再 `cargo build --release`（否则嵌入的是旧产物或空表，仅启动 warn）。
- 开发约定（composables 职责、单测/浏览器 e2e 基建、reka-ui 优先等）见本目录 `AGENTS.md`。
