import { afterEach, expect, it } from "vitest";
import { page } from "vitest/browser";
import { mountApp } from "./support/app";
import { projectFixture, providerFixture } from "./support/fixtures";
import { getRequests, resetMock, setScenario } from "./support/mock";
import type { MountedApp } from "./support/app";
import type { Scenario } from "./support/mock";
import type { components } from "@/api/schema";

type AgentView = components["schemas"]["AgentSettingsView"];
const RE_PATCH = /\/settings\/agents\//;
function profile(key: string, instructions = "", project = false): AgentView {
  const effective: AgentView["effective"] = {
    model: { mode: "parent" },
    instructions,
    tools:
      key === "title" ? { mode: "allowlist", items: [] } : { mode: "parent" },
  };
  return {
    key,
    name: key === "worker" ? "Worker 子代理" : "Title 标题生成",
    description: "测试配置",
    delegatable: key === "worker",
    tools_editable: key === "worker",
    overrides: project ? {} : { instructions },
    effective,
    global_effective: effective,
    sources: {
      model: "builtin",
      instructions: instructions ? "global" : "builtin",
      tools: "builtin",
    },
  };
}
let mounted: MountedApp | undefined;
afterEach(() => {
  mounted?.unmount();
  mounted = undefined;
});
async function text() {
  return (await page.getByRole("dialog").element()).textContent ?? "";
}
async function patches() {
  return (await getRequests()).filter(
    (r) => r.method === "PATCH" && RE_PATCH.test(r.path),
  );
}
async function open(extra: Partial<Scenario> = {}) {
  await resetMock();
  await setScenario({
    projects: [projectFixture()],
    conversations: [],
    providers: [providerFixture()],
    // 与后端 TestApp 同理：用例默认直连工具模式，code 模式用例显式开启，
    // 避免工具保存断言被自动补选的读三件套污染。
    settings: {
      llm_wire_log: true,
      mcp_tool_mode: "direct",
      code_mode_available: true,
    },
    agentSettings: {
      global: [profile("worker"), profile("title")],
      p1: [profile("worker", "全局指令", true), profile("title", "", true)],
    },
    agentTools: ["read_file", "edit_file", "write_file"].map((name) => ({
      selector: { kind: "builtin", name },
      label: name,
      group: "内置工具",
      available: true,
    })),
    ...extra,
  });
  mounted = await mountApp();
  await page.getByTitle("设置").click();
  await page.getByTitle("配置 Agents").click();
  await expect.poll(text).toContain("内置提示词说明");
}

it("全局 Worker 指令/模型/工具保存，Title 使用独立的无工具配置", async () => {
  const worker = profile("worker", "仔细检查");
  worker.overrides.tools = worker.effective.tools = {
    mode: "allowlist",
    items: [
      { kind: "builtin", name: "edit_file" },
      { kind: "builtin", name: "read_file" },
      { kind: "mcp", server_id: "m1", tool: "search" },
    ],
  };
  const title = profile("title", "使用中文");
  title.overrides.model = title.effective.model = {
    mode: "fixed",
    provider_id: "mock-prov",
    model_id: "model-a",
  };
  await open({
    agentTools: [
      ...["read_file", "edit_file", "write_file"].map((name) => ({
        selector: { kind: "builtin" as const, name },
        label: name,
        group: "内置工具",
        available: true,
      })),
      {
        selector: { kind: "mcp", server_id: "m1", tool: "search" },
        label: "search",
        group: "搜索服务",
        available: true,
      },
    ],
    agentPatchReplies: { "global/worker": worker, "global/title": title },
  });
  await page.getByRole("textbox", { name: "额外角色指令" }).fill("仔细检查");
  await page.getByTitle("Agent 工具策略").click();
  await page.getByRole("option", { name: "自定义列表" }).click();
  await page.getByRole("checkbox", { name: "edit_file", exact: true }).click();
  await expect
    .poll(
      async () =>
        (
          (await page
            .getByRole("checkbox", { name: "read_file", exact: true })
            .element()) as HTMLInputElement
        ).checked,
    )
    .toBe(true);
  await page.getByRole("checkbox", { name: "search", exact: true }).click();
  await page.getByRole("button", { name: "保存配置" }).click();
  await expect.poll(text).toContain("之后新建的子任务生效");
  await expect
    .poll(async () => (await patches())[0]?.body)
    .toEqual({
      model: { mode: "parent" },
      instructions: "仔细检查",
      tools: worker.effective.tools,
    });
  await page.getByRole("button", { name: "Title 标题生成" }).click();
  await expect.poll(text).toContain("无工具 · 单次标题生成");
  await page.getByTitle("Agent 模型策略").click();
  await page.getByRole("option", { name: "指定模型" }).click();
  await page.getByTitle("Agent 专用模型").click();
  await page
    .getByRole("option", { name: "Mock Provider / Model model-a" })
    .click();
  await page.getByRole("textbox", { name: "额外标题指令" }).fill("使用中文");
  await page.getByRole("button", { name: "保存配置" }).click();
  await expect.poll(text).toContain("已有标题不会重新生成");
  await expect
    .poll(async () => (await patches())[1]?.body)
    .toEqual({
      model: title.effective.model,
      instructions: "使用中文",
      tools: null,
    });
});

it("代码模式锁定读三件套，可清空回到无工具", async () => {
  await open({
    settings: {
      llm_wire_log: true,
      mcp_tool_mode: "code",
      code_mode_available: true,
    },
    agentTools: ["read_file", "grep", "glob", "write_file"].map((name) => ({
      selector: { kind: "builtin" as const, name },
      label: name,
      group: "内置工具",
      available: true,
    })),
    agentPatchReplies: { "global/worker": profile("worker") },
  });
  await page.getByTitle("Agent 工具策略").click();
  await page.getByRole("option", { name: "自定义列表" }).click();
  await page.getByRole("checkbox", { name: "write_file", exact: true }).click();
  // 初始挂载即生效（未切换作用域）：三个读工具自动补选并锁定。
  // 若 code-mode 状态加载失败（如初始化时序缺陷），这里不会出现锁定。
  await expect.poll(text).toContain("代码模式已启用");
  for (const name of ["read_file", "grep", "glob"]) {
    const box = page.getByRole("checkbox", { name: new RegExp(`^${name}`) });
    await expect
      .poll(async () => ((await box.element()) as HTMLInputElement).checked)
      .toBe(true);
    expect(((await box.element()) as HTMLInputElement).disabled).toBe(true);
  }
  // 锁定的三件套无法逐个取消；「清空」是回到无工具 worker 的唯一出口。
  await page.getByRole("button", { name: "清空（无工具）" }).click();
  await expect.poll(text).not.toContain("代码模式已启用");
  for (const name of ["read_file", "grep", "glob", "write_file"]) {
    await expect
      .poll(
        async () =>
          (await page.getByRole("checkbox", { name, exact: true }).element())
            .checked,
      )
      .toBe(false);
  }
  await page.getByRole("button", { name: "保存配置" }).click();
  await expect.poll(text).toContain("之后新建的子任务生效");
  await expect
    .poll(async () => (await patches())[0]?.body)
    .toEqual({
      model: { mode: "parent" },
      instructions: "",
      tools: { mode: "allowlist", items: [] },
    });
});

it("项目配置显示继承来源，恢复继承发送三个 null", async () => {
  await open({
    agentPatchReplies: { "p1/worker": profile("worker", "全局指令", true) },
  });
  await page.getByTitle("配置作用域").click();
  await page.getByRole("option", { name: "demo" }).click();
  await expect.poll(text).toContain("全局指令");
  await expect.poll(text).toContain("来自全局");
  await page.getByRole("button", { name: "全部恢复继承" }).click();
  await expect
    .poll(async () => (await patches())[0])
    .toMatchObject({
      path: "/api/projects/p1/settings/agents/worker",
      body: { model: null, instructions: null, tools: null },
    });
});

it("保存失败保留草稿，离开前可继续编辑或丢弃", async () => {
  await open({
    agentPatchReplies: { "global/worker": { error: "模型已删除" } },
  });
  await page.getByRole("textbox", { name: "额外角色指令" }).fill("未保存指令");
  await page.getByRole("button", { name: "保存配置" }).click();
  await expect.poll(text).toContain("模型已删除");
  await expect
    .poll(
      async () =>
        (
          (await page
            .getByRole("textbox", { name: "额外角色指令" })
            .element()) as HTMLTextAreaElement
        ).value,
    )
    .toBe("未保存指令");
  await page.getByRole("button", { name: "Title 标题生成" }).click();
  await expect
    .poll(
      async () => (await page.getByRole("alertdialog").element()).textContent,
    )
    .toContain("丢弃未保存修改");
  await page.getByRole("button", { name: "继续编辑" }).click();
  await expect.poll(text).toContain("有未保存修改");
  await page.getByTitle("配置作用域").click();
  await page.getByRole("option", { name: "demo" }).click();
  await page.getByRole("button", { name: "丢弃修改" }).click();
  await expect.poll(text).toContain("全局指令");
  await expect.poll(text).not.toContain("有未保存修改");
});
