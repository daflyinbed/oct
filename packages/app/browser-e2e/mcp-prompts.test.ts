import { expect, it } from "vitest";
import { page, userEvent } from "vitest/browser";
import { mountApp } from "./support/app";
import {
  conversationFixture,
  mcpNoArgPromptFixture,
  mcpPromptFixture,
  mcpResourceFixture,
  mcpServerFixture,
  projectFixture,
} from "./support/fixtures";
import { getRequests, resetMock, setScenario } from "./support/mock";

const COMPOSER = "Ask Oct to do something...";

// 提到模块作用域,避免每次调用重新编译正则
const RE_STATUS_OPTION = /status/;

/** mock 请求日志里发往聊天端点的 POST 数（断言「填入不自动发送」）。 */
async function chatPostCount(): Promise<number> {
  const requests = await getRequests();
  return requests.filter(
    (r) => r.method === "POST" && r.path === "/api/conversations/c1/messages",
  ).length;
}

/** mock 请求日志里某路径的 POST body 列表。 */
async function postBodies(path: string): Promise<unknown[]> {
  const requests = await getRequests();
  return requests
    .filter((r) => r.method === "POST" && r.path === path)
    .map((r) => r.body);
}

async function composerValue(): Promise<string> {
  const el = (await page
    .getByPlaceholder(COMPOSER)
    .element()) as HTMLTextAreaElement | null;
  return el?.value ?? "";
}

async function listboxText(): Promise<string> {
  const el = await page.getByRole("listbox").element();
  return el.textContent ?? "";
}

async function ensureFired(): Promise<boolean> {
  const requests = await getRequests();
  return requests.some(
    (r) => r.method === "POST" && r.path === "/api/projects/p1/mcp/ensure",
  );
}

/** 播种并进入会话:一个 echo server(connected 前置),带 prompts/resources。 */
async function boot() {
  await resetMock();
  await setScenario({
    projects: [projectFixture()],
    conversations: [conversationFixture()],
    providers: [],
    mcpServers: [
      mcpServerFixture("m1", {
        name: "echo",
        status: "connected",
        tool_count: 1,
        tools: [{ name: "echo_tool", description: null }],
        prompts: [
          mcpNoArgPromptFixture("status", {
            description: "report status",
          }),
          mcpPromptFixture("deploy", {
            description: "deploy checklist",
            arguments: [
              {
                name: "env",
                description: "target environment",
                required: true,
              },
            ],
          }),
        ],
        resources: [
          mcpResourceFixture("memo://notes", {
            name: "notes",
            description: "meeting notes",
          }),
          mcpResourceFixture("file://logo.png", {
            name: "logo",
            mime_type: "image/png",
          }),
        ],
      }),
    ],
    mcpPromptRenders: {
      m1: {
        status: {
          messages: [{ role: "user", text: "Report status" }],
        },
        deploy: {
          messages: [
            { role: "user", text: "Deploy to prod" },
            { role: "assistant", text: "Checklist ready" },
          ],
        },
      },
    },
    mcpResourceReads: {
      m1: {
        "memo://notes": { mime_type: "text/markdown", text: "# Notes" },
        "file://logo.png": {
          mime_type: "image/png",
          blob_base64: "aVBuZw==",
        },
      },
    },
  });
  const mounted = await mountApp();
  await mounted.router.push("/conversation/c1");
  await expect.element(page.getByPlaceholder(COMPOSER)).toBeInTheDocument();
  await expect.poll(ensureFired).toBe(true);
  return mounted;
}

/**
 * 斜杠命令菜单:输入 / 触发(prompt 按 server 分组),查询词过滤;带参
 * prompt 弹参数表单,渲染结果**填入输入框**而非自动发送;无参 prompt 直接
 * 渲染填入;Escape 收起本次激活。
 */
it("slash menu: 无参与带参 prompt 渲染后填入输入框,不自动发送", async () => {
  const mounted = await boot();
  const composer = page.getByPlaceholder(COMPOSER);

  // `/` 触发菜单:server 分组(echo)列出两个 prompt。
  await composer.fill("/");
  await expect.element(page.getByRole("listbox")).toBeInTheDocument();
  await expect.poll(listboxText).toContain("echo");
  await expect.poll(listboxText).toContain("status");
  await expect.poll(listboxText).toContain("deploy");

  // 查询词过滤:只剩 deploy。
  await composer.fill("/dep");
  await expect.poll(listboxText).not.toContain("status");
  await expect.poll(listboxText).toContain("deploy");

  // 键盘导航 + Enter 选中带参 prompt:弹参数表单(必填校验 + 提交)。
  await composer.fill("/");
  await userEvent.keyboard("{ArrowDown}"); // 第一项 status → 第二项 deploy
  await userEvent.keyboard("{Enter}");
  await expect.element(page.getByRole("dialog")).toBeInTheDocument();
  await expect
    .poll(
      async () => (await page.getByRole("dialog").element()).textContent ?? "",
    )
    .toContain("/deploy");
  await page.getByPlaceholder("target environment").fill("prod");
  await page.getByRole("button", { name: "渲染" }).click();
  await expect
    .poll(async () => (await page.getByRole("dialog").query()) === null)
    .toBe(true);

  // 渲染请求携带参数;两条 message 以 user:/assistant: 前缀拼入输入框。
  await expect
    .poll(
      async () =>
        (await postBodies("/api/mcp/servers/m1/prompts/deploy")).length,
    )
    .toBe(1);
  const bodies = await postBodies("/api/mcp/servers/m1/prompts/deploy");
  expect(bodies[0]).toEqual({ arguments: { env: "prod" } });
  await expect
    .poll(composerValue)
    .toBe("user: Deploy to prod\n\nassistant: Checklist ready");

  // 菜单选择绝不触发发送:聊天端点零请求。
  expect(await chatPostCount()).toBe(0);

  // 无参 prompt:单击选项直接渲染;单条 user 消息不加前缀。
  await composer.fill("/");
  await expect.element(page.getByRole("listbox")).toBeInTheDocument();
  await page.getByRole("option", { name: RE_STATUS_OPTION }).click();
  await expect.poll(composerValue).toBe("Report status");
  expect(await chatPostCount()).toBe(0);

  // Escape 收起本次激活:菜单消失,输入保留。
  await composer.fill("/");
  await expect.element(page.getByRole("listbox")).toBeInTheDocument();
  await userEvent.keyboard("{Escape}");
  await expect
    .poll(async () => (await page.getByRole("listbox").query()) === null)
    .toBe(true);

  mounted.unmount();
});

/**
 * MCP 面板:Prompts 只读清单(使用入口指向斜杠命令)与 Resources 分区
 * (点击查看:文本原样、二进制占位)。
 */
it("mcp panel: prompts 清单与 resources 面板内查看", async () => {
  const mounted = await boot();

  // 经状态栏指示器打开 echo 的面板。
  await page.getByTitle("MCP 服务器状态").click();
  await page.getByText("echo", { exact: true }).hover();
  await page.getByText("查看加载的工具").click();

  const panelText = async () => {
    const el = document.querySelector("#chat-panel");
    return el?.textContent ?? "";
  };

  // 工具分区之外新增 Prompts 分区:名字带 / 前缀、参数与使用提示。
  await expect.poll(panelText).toContain("Prompts · 2");
  await expect.poll(panelText).toContain("/status");
  await expect.poll(panelText).toContain("/deploy");
  await expect.poll(panelText).toContain("env");
  await expect.poll(panelText).toContain("在聊天输入框输入");

  // Resources 分区:列出 uri/name/mime;点击 notes 展开文本内容。
  await expect.poll(panelText).toContain("Resources · 2");
  await expect.poll(panelText).toContain("memo://notes");
  await expect.poll(panelText).toContain("image/png");
  await page.getByText("notes", { exact: true }).click();
  await expect.poll(panelText).toContain("# Notes");

  // 二进制 resource:占位说明(base64 长度),不渲染原始数据。
  await page.getByText("logo", { exact: true }).click();
  await expect.poll(panelText).toContain("二进制内容");
  await expect.poll(panelText).toContain("aVBuZw==".length.toString());
  await expect.poll(panelText).not.toContain("aVBuZw==");
  // notes 的文本内容已被切走(同屏只展开一条)。
  await expect.poll(panelText).not.toContain("# Notes");

  mounted.unmount();
});
