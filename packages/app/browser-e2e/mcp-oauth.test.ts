import { expect, it } from "vitest";
import { page } from "vitest/browser";
import { mountApp } from "./support/app";
import {
  mcpOauthSummaryFixture,
  mcpServerFixture,
  projectFixture,
} from "./support/fixtures";
import { getRequests, resetMock, setScenario } from "./support/mock";
import type { MountedApp } from "./support/app";

// MCP OAuth(http transport)登录链路:start → window.open(stub)→ 直接
// fetch authorization_url(mock 把授权服务器折叠成指回自身 callback 的
// URL,驱动"回调落库")→ 轮询就绪 → 状态徽章;退出登录;start 失败透出。

/** 设置对话框的完整文本;UI 是异步的,断言一律走 expect.poll。 */
async function modalText(): Promise<string> {
  const dialog = await page.getByRole("dialog").element();
  return dialog.textContent ?? "";
}

/** http 行 fixture(默认未授权)。 */
function httpServer(
  id: string,
  overrides: Partial<ReturnType<typeof mcpServerFixture>> = {},
) {
  return mcpServerFixture(id, {
    name: id,
    transport: "http",
    command: null,
    url: `https://api.example.com/${id}/mcp`,
    ...overrides,
  });
}

/** 重置 mock、播种 http server 行,挂载 App 并进到指定行的编辑表单。 */
async function openEditForm(
  servers: unknown[],
  targetName: string,
  opts: {
    mcpOauthStarts?: Record<string, { status?: number; error?: string }>;
  } = {},
): Promise<MountedApp> {
  await resetMock();
  await setScenario({
    projects: [projectFixture()],
    conversations: [],
    providers: [],
    mcpServers: servers,
    mcpOauthStarts: opts.mcpOauthStarts,
  });
  const mounted = await mountApp();
  await page.getByTitle("设置").click();
  await expect.poll(modalText).toContain("LLM Providers");
  await page.getByTitle("配置 MCP Servers").click();
  await expect
    .poll(modalText)
    .toContain("为 Agent 接入外部工具服务器（stdio / http）");
  await page.getByTitle(`配置 ${targetName}`).click();
  await expect.poll(modalText).toContain("Edit MCP Server");
  return mounted;
}

// 提到模块作用域,避免每次调用重新编译正则
const RE_OAUTH_START = /\/oauth\/start$/;
const RE_OAUTH_DELETE = /\/oauth$/;

/** mock 日志里指定方法+路径正则的请求。 */
async function requestsOf(
  method: string,
  pathRe: RegExp,
): Promise<{ path: string; body: unknown }[]> {
  const requests = await getRequests();
  return requests.filter((r) => r.method === method && pathRe.test(r.path)) as {
    path: string;
    body: unknown;
  }[];
}

it("mcp oauth: 登录全流程——start → 打开授权页(stub) → 回调就绪 → 已授权徽章", async () => {
  const mounted = await openEditForm([httpServer("m1")], "m1");

  // 未授权徽章 + 登录按钮在场
  await expect.poll(modalText).toContain("OAuth");
  await expect.poll(modalText).toContain("未授权");

  // stub window.open:捕获 authorization_url,不真的开新页
  const opens: string[] = [];
  const originalOpen = window.open;
  window.open = ((url?: string | URL) => {
    opens.push(String(url));
    return null;
  }) as typeof window.open;
  try {
    await page.getByRole("button", { name: "登录" }).click();

    // start 已发、URL 已被"打开"
    await expect
      .poll(async () => (await requestsOf("POST", RE_OAUTH_START)).length)
      .toBeGreaterThan(0);
    await expect.poll(() => opens.length).toBeGreaterThan(0);

    // "浏览器"侧:直接导航 authorization_url(mock 的 AS 即回调),
    // 拿到成功页
    const callbackResponse = await fetch(opens[0]!);
    expect(callbackResponse.status).toBe(200);
    expect(await callbackResponse.text()).toContain("授权成功");

    // 前端轮询(1s 间隔)就绪后徽章翻为已授权;默认 poll 超时拉宽兜住轮询节奏
    await expect.poll(modalText, { timeout: 8_000 }).toContain("已授权");
    await expect.poll(modalText).toContain("有效期至");
    await expect.poll(modalText).not.toContain("等待授权完成");
  } finally {
    window.open = originalOpen;
  }
  mounted.unmount();
});

it("mcp oauth: 已授权/需重新登录徽章、退出登录、start 失败透出后端错误", async () => {
  const mounted = await openEditForm(
    [
      httpServer("m1", {
        oauth: mcpOauthSummaryFixture(),
      }),
      httpServer("m2", {
        oauth: mcpOauthSummaryFixture({ expires_at: "2000-01-01T00:00:00" }),
      }),
    ],
    "m1",
    {
      // m2 的 start 剧本:502 发现失败(对齐真实后端的 BadGateway 文案)
      mcpOauthStarts: {
        m2: { status: 502, error: "OAuth discovery failed: no metadata" },
      },
    },
  );

  // m1:已授权徽章 + 有效期 + 退出登录
  await expect.poll(modalText).toContain("已授权");
  await expect.poll(modalText).toContain("有效期至");
  await expect
    .poll(async () => {
      const el = await page.getByRole("button", { name: "退出登录" }).query();
      return el !== null;
    })
    .toBe(true);

  // 退出登录:DELETE 打到 mock,徽章回到未授权
  await page.getByRole("button", { name: "退出登录" }).click();
  await expect
    .poll(async () => (await requestsOf("DELETE", RE_OAUTH_DELETE)).length)
    .toBeGreaterThan(0);
  await expect.poll(modalText, { timeout: 8_000 }).toContain("未授权");

  // m2:过期 → 需重新登录徽章;登录失败(502)错误文案透出
  await page.getByTitle("返回").click();
  await page.getByTitle("配置 m2").click();
  await expect.poll(modalText, { timeout: 8_000 }).toContain("需重新登录");
  await page.getByRole("button", { name: "重新登录" }).click();
  await expect
    .poll(modalText, { timeout: 8_000 })
    .toContain("OAuth discovery failed: no metadata");

  mounted.unmount();
});
