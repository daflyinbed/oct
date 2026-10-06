import { afterEach, expect, it } from "vitest";
import { page } from "vitest/browser";
import { mountApp } from "./support/app";
import {
  finishEvent,
  projectFixture,
  providerFixture,
  toolCallStart,
  toolResult,
} from "./support/fixtures";
import { bootConversation, chatText, sendChatMessage } from "./support/harness";
import { getRequests, openGate, resetMock, setScenario } from "./support/mock";
import type { MountedApp } from "./support/app";
import type { components } from "@/api/schema";

let mounted: MountedApp | undefined;
const CODE_CARD = /代码执行/;
const INHERIT = /继承全局/;
afterEach(() => {
  mounted?.unmount();
  mounted = undefined;
});

it("代码卡显示嵌套调用，运行事件重放后仍只有一条记录", async () => {
  const call: components["schemas"]["NestedCall"] = {
    id: "code1/1",
    name: "mcp__api__search",
    title: "api / search",
    status: "running",
    error: null,
  };
  const update = (status: string): components["schemas"]["AgentEvent"] => ({
    type: "nested_tool_update",
    data: {
      parent_call_id: "code1",
      call: { ...call, status, duration_ms: status === "running" ? null : 14 },
    },
  });
  mounted = await bootConversation([
    {
      type: "events",
      events: [
        toolCallStart(
          "code1",
          "execute_code",
          "Code mode",
          '{"code":"text(42)"}',
        ),
        update("running"),
      ],
    },
    { type: "gate", name: "nested" },
    {
      type: "events",
      events: [
        update("completed"),
        update("completed"),
        toolResult("code1", "42", {
          kind: "code_mode",
          calls: [
            { ...call, status: "completed", duration_ms: 14 },
            {
              ...call,
              id: "code1/2",
              title: "api / denied",
              status: "failed",
              duration_ms: 48,
              error_code: "permission_denied",
              error: "Permission denied by the user",
            },
          ],
        }),
        finishEvent(),
      ],
    },
  ]);
  await sendChatMessage("code");
  await expect.poll(chatText).toContain("代码执行");
  await page.getByRole("button", { name: CODE_CARD }).click();
  await expect.poll(chatText).toContain("api / search");
  await openGate("nested");
  await expect.poll(chatText).toContain("完成");
  await expect.poll(chatText).toContain("14 ms");
  await expect.poll(chatText).toContain("48 ms");
  await expect.poll(chatText).toContain("permission_denied");
  await expect
    .element(page.getByText("api / search", { exact: true }))
    .toBeInTheDocument();
  await expect.poll(chatText).toContain("text(42)");
});

it("设置中切换全局调用方式，项目可以覆盖再恢复继承", async () => {
  await resetMock();
  await setScenario({
    projects: [projectFixture()],
    conversations: [],
    providers: [providerFixture()],
  });
  mounted = await mountApp();
  await page.getByTitle("设置").click();
  await page.getByTitle("工具调用方式").click();
  await page.getByRole("option", { name: "代码模式", exact: true }).click();
  await expect
    .poll(async () =>
      (await getRequests())
        .filter((r) => r.method === "PUT" && r.path === "/api/settings")
        .map((r) => r.body),
    )
    .toEqual([{ mcp_tool_mode: "code" }]);
  await page.getByTitle("配置作用域").click();
  await page.getByRole("option", { name: "demo", exact: true }).click();
  await page.getByTitle("工具调用方式").click();
  await page.getByRole("option", { name: "直接调用", exact: true }).click();
  await expect
    .poll(async () =>
      (await getRequests())
        .filter((r) => r.method === "PUT" && r.path.endsWith("/settings/mcp"))
        .map((r) => r.body),
    )
    .toEqual([{ mcp_tool_mode: "direct" }]);
  await page.getByTitle("工具调用方式").click();
  await page.getByRole("option", { name: INHERIT }).click();
  await expect
    .poll(async () =>
      (await getRequests())
        .filter((r) => r.method === "PUT" && r.path.endsWith("/settings/mcp"))
        .map((r) => r.body),
    )
    .toEqual([{ mcp_tool_mode: "direct" }, { mcp_tool_mode: null }]);
});
