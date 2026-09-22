import { expect, it } from "vitest";
import { page } from "vitest/browser";
import { mountApp } from "./support/app";
import { mcpServerFixture, projectFixture } from "./support/fixtures";
import { getRequests, resetMock, setScenario } from "./support/mock";
import type { MountedApp } from "./support/app";

// 提到模块作用域,避免每次调用重新编译正则
const RE_MCP_API = /^\/api\/mcp\/servers\//;
const RE_ARGS_PLACEHOLDER = /server-filesystem/;
// 集合端点(无 id 后缀),与 RE_MCP_API 的单项路由区分
const RE_MCP_LIST = /^\/api\/mcp\/servers$/;

/** 设置对话框的完整文本;UI 是异步的,断言一律走 expect.poll。 */
async function modalText(): Promise<string> {
  const dialog = await page.getByRole("dialog").element();
  return dialog.textContent ?? "";
}

/** 重置 mock、播种 MCP 初始行,挂载 App 并经设置入口导航到 MCP 视图。 */
async function openMcpSettings(seed: unknown[] = []): Promise<MountedApp> {
  await resetMock();
  await setScenario({
    projects: [projectFixture()],
    conversations: [],
    providers: [],
    mcpServers: seed,
  });
  const mounted = await mountApp();
  await page.getByTitle("设置").click();
  // AI 根页出现后进 MCP 子视图
  await expect.poll(modalText).toContain("LLM Providers");
  await page.getByTitle("配置 MCP Servers").click();
  await expect
    .poll(modalText)
    .toContain("为 Agent 接入外部工具服务器（stdio）");
  return mounted;
}

async function mcpRequests(
  method: string,
  pathRe: RegExp,
): Promise<{ path: string; body: unknown }[]> {
  const requests = await getRequests();
  return requests.filter((r) => r.method === method && pathRe.test(r.path)) as {
    path: string;
    body: unknown;
  }[];
}

/** mock 日志里 GET /api/mcp/servers(列表)的累计次数。 */
async function mcpListCount(): Promise<number> {
  return (await mcpRequests("GET", RE_MCP_LIST)).length;
}

it("mcp settings: 新增 → 列表(未连接) → 切 enabled → 编辑 command → 删除后空", async () => {
  const mounted = await openMcpSettings();

  await expect.poll(modalText).toContain("尚未配置任何 MCP server");

  // 新增:顶栏 Add Server 进表单
  await page.getByRole("button", { name: "Add Server" }).first().click();
  await expect.poll(modalText).toContain("Add MCP Server");

  await page.getByPlaceholder("e.g. filesystem").fill("fs");
  await page.getByPlaceholder("e.g. npx").fill("npx");
  await page
    .getByPlaceholder(RE_ARGS_PLACEHOLDER)
    .fill("-y\n@modelcontextprotocol/server-filesystem");
  await page.getByPlaceholder("API_TOKEN=secret").fill("API_TOKEN=secret");
  // 作用域:全局 → 项目 demo
  await page.getByTitle("作用域").click();
  await page.getByRole("option", { name: "demo" }).click();
  await page.getByRole("button", { name: "Create Server" }).click();

  // 列表出现:name、作用域徽标(项目名)、not_connected 状态
  await expect.poll(modalText).toContain("fs");
  await expect.poll(modalText).toContain("未连接");
  await expect.poll(modalText).toContain("demo");

  // 切 enabled:开关翻转,PATCH 带enabled:false 落到 mock
  await page.getByRole("switch", { name: "启用 fs" }).click();
  await expect
    .poll(async () => {
      const patches = await mcpRequests("PATCH", RE_MCP_API);
      return patches.some(
        (r) => (r.body as { enabled?: boolean } | null)?.enabled === false,
      );
    })
    .toBe(true);
  await expect
    .poll(async () => {
      const el = await page.getByRole("switch", { name: "启用 fs" }).element();
      return el.getAttribute("aria-checked");
    })
    .toBe("false");

  // 编辑:command 改为 uvx,保存后列表副文本跟着变
  await page.getByTitle("配置 fs").click();
  await expect.poll(modalText).toContain("Edit MCP Server");
  await expect
    .poll(async () => {
      const el = (await page
        .getByPlaceholder("e.g. npx")
        .element()) as HTMLInputElement | null;
      return el?.value ?? "";
    })
    .toBe("npx");
  await page.getByPlaceholder("e.g. npx").fill("uvx");
  await page.getByRole("button", { name: "Save Changes" }).click();
  await expect.poll(modalText).toContain("uvx");

  // 删除:列表回到空态
  await page.getByTitle("删除 fs").click();
  await expect.poll(modalText).toContain("尚未配置任何 MCP server");

  mounted.unmount();
});

it("mcp settings: 重名创建 409,错误信息透出;failed 状态显示错误副文本", async () => {
  const mounted = await openMcpSettings([
    mcpServerFixture("m1", {
      name: "fs",
      status: "failed",
      error: "spawn failed: ENOENT",
    }),
  ]);

  // 已有行:failed 徽标 + 错误副文本
  await expect.poll(modalText).toContain("fs");
  await expect.poll(modalText).toContain("spawn failed: ENOENT");

  // 重名创建:mock 回 409,表单展示服务端 message
  await page.getByRole("button", { name: "Add Server" }).first().click();
  await page.getByPlaceholder("e.g. filesystem").fill("fs");
  await page.getByPlaceholder("e.g. npx").fill("npx");
  await page.getByRole("button", { name: "Create Server" }).click();

  await expect.poll(modalText).toContain("MCP server 'fs' already exists");

  mounted.unmount();
});

it("mcp settings: 关闭后重开(停在 mcp 视图)重新拉列表并渲染新状态;切走切回仍刷新", async () => {
  const mounted = await openMcpSettings([mcpServerFixture("m1")]);

  // 初始 not_connected;该断言通过时首次列表请求必然已落进 mock 日志,
  // 此刻读到的计数是稳定的基线
  await expect.poll(modalText).toContain("未连接");
  const before = await mcpListCount();

  // 关闭弹窗:视图状态机与模块级列表缓存跨开合保留
  await page.getByTitle("关闭（Esc）").click();
  await expect
    .poll(async () => (await page.getByRole("dialog").query()) === null)
    .toBe(true);

  // 弹窗外的状态变化:PATCH 与真实后端一致不接受 status,故整体重放
  // scenario 把该行改为 connected + 1 个工具
  await setScenario({
    projects: [projectFixture()],
    conversations: [],
    providers: [],
    mcpServers: [
      mcpServerFixture("m1", { status: "connected", tool_count: 1 }),
    ],
  });

  // 重开:直接落回 mcp 视图(view.name 未变)
  await page.getByTitle("设置").click();
  await expect
    .poll(async () => (await page.getByRole("dialog").query()) !== null)
    .toBe(true);
  await expect
    .poll(modalText)
    .toContain("为 Agent 接入外部工具服务器（stdio）");
  // a) 重开动作恰好多发一次列表请求:证明发了请求,同时防重复
  await expect.poll(mcpListCount).toBe(before + 1);
  // b) 渲染的是新状态而非模块级缓存的旧状态
  await expect.poll(modalText).toContain("已连接 · 1 个工具");
  expect(await mcpListCount()).toBe(before + 1);

  // 回归:视图切换路径仍然刷新(经编辑视图绕一圈回到列表)
  await page.getByRole("button", { name: "Add Server" }).first().click();
  await expect.poll(modalText).toContain("Add MCP Server");
  await page.getByRole("button", { name: "Cancel" }).click();
  await expect.poll(mcpListCount).toBe(before + 2);
  await expect.poll(modalText).toContain("已连接 · 1 个工具");

  mounted.unmount();
});
