import { expect, it } from "vitest";
import { page, userEvent } from "vitest/browser";
import { mountApp } from "./support/app";
import {
  conversationFixture,
  mcpServerFixture,
  projectFixture,
} from "./support/fixtures";
import { getRequests, resetMock, setScenario } from "./support/mock";

// 提到模块作用域,避免每次调用重新编译正则
const RE_ENSURE = /^\/api\/projects\/p1\/mcp\/ensure$/;

/** mock 日志里 POST ensure 的累计次数。 */
async function ensureCount(): Promise<number> {
  const requests = await getRequests();
  return requests.filter((r) => r.method === "POST" && RE_ENSURE.test(r.path))
    .length;
}

/** 底栏指示器按钮内汇总状态点的 class(点在按钮内部的 span 上)。 */
async function summaryDotClass(): Promise<string> {
  const el = await page.getByTitle("MCP 服务器状态").element();
  const dot = el.querySelector("span");
  return dot?.className ?? "";
}

/** 打开的主菜单文本(子菜单展开后取 last;reka 的 menu content 都是 role=menu)。 */
async function menuText(which: "first" | "last" = "first"): Promise<string> {
  const el =
    which === "first"
      ? await page.getByRole("menu").first().element()
      : await page.getByRole("menu").last().element();
  return el.textContent ?? "";
}

async function menuCount(): Promise<number> {
  return page.getByRole("menu").elements().length;
}

/**
 * 底栏 MCP 指示器:选中会话即对建连端点恰好发一次请求,not_connected 行
 * 被服务端翻成 connected;菜单按作用域分组列出各 server,行悬停弹出子菜单,
 * 首项按状态切换(connected → 查看加载的工具;failed → 打开错误日志),
 * 均打开该 server 独占的面板 tab;「配置…」深链设置弹窗的编辑表单。
 */
it("mcp indicator: 子菜单按状态打开各 server 的面板,状态行与分组就位", async () => {
  await resetMock();
  await setScenario({
    projects: [projectFixture()],
    conversations: [conversationFixture()],
    providers: [],
    mcpServers: [
      mcpServerFixture("m1", { name: "echo" }), // not_connected → ensure 后 connected
      mcpServerFixture("m2", {
        name: "broken",
        project_id: "p1",
        status: "failed",
        error: "spawn failed: ENOENT",
      }),
    ],
  });

  const mounted = await mountApp();
  await mounted.router.push("/conversation/c1");

  // 进入会话恰好触发一次 ensure
  await expect.poll(ensureCount).toBe(1);

  // 打开菜单:全局组(echo)在上,项目组(demo 标签 + broken)在下
  await page.getByTitle("MCP 服务器状态").click();
  await expect.poll(menuCount).toBe(1);
  await expect
    .poll(async () => {
      const text = await menuText("first");
      const at = (needle: string) => text.indexOf(needle);
      return (
        at("全局") !== -1 &&
        at("全局") < at("echo") &&
        at("echo") < at("demo") &&
        at("demo") < at("broken")
      );
    })
    .toBe(true);
  await expect.poll(() => menuText("first")).toContain("1 个工具");
  await expect.poll(() => menuText("first")).toContain("连接失败");

  // 悬停 echo 行(connected)→ 子菜单:查看加载的工具 + 配置 + 状态行
  await page.getByText("echo", { exact: true }).hover();
  await expect.poll(menuCount).toBe(2);
  await expect.poll(() => menuText("last")).toContain("查看加载的工具");
  await expect.poll(() => menuText("last")).toContain("配置…");
  await expect.poll(() => menuText("last")).toContain("已连接 · 1 个工具");

  // Escape 收起菜单(模态菜单的遮罩会吞掉外部点击;同一菜单内悬停切行
  // 存在子菜单切换时序,重开更稳)
  await userEvent.keyboard("{Escape}");
  await expect.poll(menuCount).toBe(0);

  // 重新打开,悬停 broken 行(failed)→ 首项变为「打开错误日志」
  await page.getByTitle("MCP 服务器状态").click();
  await expect.poll(menuCount).toBe(1);
  await page.getByText("broken", { exact: true }).hover();
  await expect.poll(() => menuText("last")).toContain("spawn failed: ENOENT");
  await expect.poll(() => menuText("last")).toContain("打开错误日志");
  await expect.poll(menuCount).toBe(2);

  // 「配置…」(先于面板打开,避免面板与菜单的 server 名重名):打开设置
  // 弹窗并直接落到该 server 的编辑表单
  await page.getByText("配置…").click();
  await expect.poll(menuCount).toBe(0);
  await expect
    .poll(async () => (await page.getByRole("dialog").query()) !== null)
    .toBe(true);
  await expect
    .poll(() => document.body.textContent ?? "")
    .toContain("Edit MCP Server");
  const nameInput = (await page
    .getByPlaceholder("e.g. filesystem")
    .element()) as HTMLInputElement | null;
  expect(nameInput?.value).toBe("broken");

  // 关闭设置弹窗,从 broken 行打开错误日志面板
  await page.getByTitle("关闭（Esc）").click();
  await expect
    .poll(async () => (await page.getByRole("dialog").query()) === null)
    .toBe(true);
  await page.getByTitle("MCP 服务器状态").click();
  await expect.poll(menuCount).toBe(1);
  await page.getByText("broken", { exact: true }).hover();
  await expect.poll(() => menuText("last")).toContain("打开错误日志");
  await expect.poll(menuCount).toBe(2);
  await page.getByText("打开错误日志").click();
  await expect.poll(menuCount).toBe(0);

  // 每个 server 独占一个面板:标题为 server 名,错误日志即面板主体。
  // MCP tab 渲染在 chat 面板内但不在 ChatView 的 <main> 里,取 #chat-panel。
  const panelText = async () =>
    document.querySelector("#chat-panel")?.textContent ?? "";
  await expect.poll(panelText).toContain("最近错误");
  await expect.poll(panelText).toContain("spawn failed: ENOENT");

  // 再从 echo 行打开工具面板:独立 tab,面板切换为 echo 的工具清单
  await page.getByTitle("MCP 服务器状态").click();
  await expect.poll(menuCount).toBe(1);
  await page.getByText("echo", { exact: true }).hover();
  await expect.poll(() => menuText("last")).toContain("查看加载的工具");
  await expect.poll(menuCount).toBe(2);
  await page.getByText("查看加载的工具").click();
  await expect.poll(menuCount).toBe(0);
  await expect.poll(panelText).toContain("echo_tool");
  await expect.poll(panelText).toContain("mock tool of echo");
  // broken 的错误块已被切走:两个面板互不混叠
  await expect.poll(panelText).not.toContain("spawn failed: ENOENT");

  mounted.unmount();
});

it("mcp indicator: 无会话上下文时不显示,进入会话后出现且状态点变绿", async () => {
  await resetMock();
  await setScenario({
    projects: [projectFixture()],
    conversations: [conversationFixture()],
    providers: [],
    mcpServers: [mcpServerFixture("m1", { name: "echo" })],
  });

  const mounted = await mountApp();
  // 同一 iframe 内测试共享 history:上一个用例停在 /conversation/c1,
  // 挂载即处于会话上下文。等侧栏会话入口出现(初始化数据落地、ensure
  // 恰好触发一次)后,计数才有稳定基线。
  await expect.element(page.getByText("New Chat").first()).toBeInTheDocument();
  await expect.poll(ensureCount).toBe(1);

  // 回根路由:无会话上下文,指示器不在场
  await mounted.router.push("/");
  await expect.element(page.getByTitle("MCP 服务器状态").query()).toBeNull();

  // 再进会话:指示器出现,恰好多发一次 ensure
  await mounted.router.push("/conversation/c1");
  await expect.element(page.getByTitle("MCP 服务器状态")).toBeInTheDocument();
  await expect.poll(ensureCount).toBe(2);

  // 全部已连接(mock 把 not_connected 翻成 connected)→ 绿点
  await expect.poll(summaryDotClass).toContain("bg-success-10");

  mounted.unmount();
});
