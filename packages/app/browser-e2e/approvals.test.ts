import { expect, it } from "vitest";
import { page } from "vitest/browser";
import { mountApp } from "./support/app";
import {
  conversationFixture,
  finishEvent,
  projectFixture,
  providerFixture,
  textDelta,
  toolCallStart,
} from "./support/fixtures";
import {
  chatText,
  COMPOSER_SELECTOR,
  sendChatMessage,
} from "./support/harness";
import { getRequests, resetMock, setScenario } from "./support/mock";
import type { components } from "@/api/schema";

type AgentEvent = components["schemas"]["AgentEvent"];

it.each([
  ["allow", "允许一次"],
  ["allow_always", "总是允许"],
  ["deny", "拒绝"],
])(
  "forwards a worker approval to the parent and continues after %s",
  async (decision, button) => {
    const approval: components["schemas"]["PendingApprovalDto"] = {
      approval_id: "approval-worker",
      agent_id: "worker-a",
      run_id: "run-worker",
      call_id: "write-worker",
      tool: "write_file",
      kind: "write",
      title: "/tmp/result.txt",
      path: "/tmp/result.txt",
      command: null,
      outside: true,
      rule_suggestion: "/tmp/**",
      created_at: "2026-10-04T00:00:00",
    };
    const task: AgentEvent = {
      type: "subagent_updated",
      data: {
        agent_id: "worker-a",
        run_id: "run-worker",
        execution_epoch: 1,
        parent_call_id: "task-a",
        description: "整理测试结果",
        status: "running",
      },
    };
    await resetMock();
    await setScenario({
      projects: [projectFixture()],
      conversations: [conversationFixture()],
      providers: [providerFixture()],
      approvals: { c1: [approval] },
      approvalGates: { "approval-worker": "decided" },
      scripts: {
        c1: [
          {
            type: "events",
            events: [
              toolCallStart(
                "task-a",
                "task",
                "整理测试结果",
                JSON.stringify({
                  description: "整理测试结果",
                  prompt: "do it",
                }),
              ),
              task,
              {
                type: "approval_requested",
                data: approval,
              } satisfies AgentEvent,
            ],
          },
          { type: "gate", name: "decided" },
          {
            type: "events",
            events: [
              {
                type: "approval_resolved",
                data: {
                  approval_id: approval.approval_id,
                  agent_id: "worker-a",
                  outcome: decision === "deny" ? "denied" : "approved",
                  rule_id: null,
                },
              } satisfies AgentEvent,
              textDelta("父任务继续"),
              finishEvent(),
            ],
          },
        ],
      },
    });
    const mounted = await mountApp();
    try {
      await mounted.router.push("/conversation/c1");
      await expect
        .element(page.getByPlaceholder(COMPOSER_SELECTOR))
        .toBeInTheDocument();
      await sendChatMessage("委派整理");
      await expect.poll(chatText).toContain("子代理 · 整理测试结果");
      await expect
        .poll(() => page.getByTestId("task-card").element().textContent)
        .toContain("等待审批 · 1");
      if (decision === "allow_always") {
        await page
          .getByPlaceholder(
            "绝对路径 glob / 命令前缀（可编辑后随「总是允许」保存）",
          )
          .fill("/tmp/results/**");
      }
      await page.getByRole("button", { name: button, exact: true }).click();
      await expect.poll(chatText).toContain("父任务继续");
      await expect
        .element(page.getByRole("button", { name: "允许一次", exact: true }))
        .not.toBeInTheDocument();
      await expect
        .element(page.getByTitle("发送（Enter）"))
        .toBeInTheDocument();
      const request = (await getRequests()).find(
        (entry) =>
          entry.method === "POST" &&
          entry.path === "/api/conversations/c1/approvals/approval-worker",
      );
      expect(request?.body).toEqual({
        decision,
        pattern: decision === "allow_always" ? "/tmp/results/**" : null,
      });
    } finally {
      mounted.unmount();
    }
  },
);
