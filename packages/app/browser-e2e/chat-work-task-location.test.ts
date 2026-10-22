import { afterEach, expect, it, vi } from "vitest";
import { page } from "vitest/browser";
import { useChat } from "@/composables/useChat";
import { mountApp } from "./support/app";
import {
  conversationFixture,
  projectFixture,
  providerFixture,
  storedMessage,
} from "./support/fixtures";
import { chatText, COMPOSER_SELECTOR } from "./support/harness";
import { resetMock, setScenario } from "./support/mock";
import type { MountedApp } from "./support/app";
import type { components } from "@/api/schema";

let mounted: MountedApp | null = null;
afterEach(() => {
  mounted?.unmount();
  mounted = null;
  useChat().clearMessages();
  vi.restoreAllMocks();
});

const task: components["schemas"]["BackgroundTaskDto"] = {
  id: "bg-task",
  conversation_id: "c1",
  command: "echo background-result",
  working_dir: "/tmp/demo",
  status: "completed",
  exit_code: 0,
  output_path: "/tmp/exec/bg-task.log",
  output_bytes: 18,
  error: null,
  created_at: "2026-10-21T00:00:00",
  finished_at: "2026-10-21T00:00:01",
};

async function bootTaskHistory() {
  await resetMock();
  await setScenario({
    projects: [projectFixture()],
    conversations: [conversationFixture()],
    providers: [providerFixture()],
    backgroundTasks: { c1: [task] },
    messages: {
      c1: [
        storedMessage("user", [{ type: "text", text: "先检查" }]),
        storedMessage("assistant", [
          { type: "reasoning", text: "不相关的工作过程" },
          { type: "text", text: "检查完成。" },
        ]),
        storedMessage("user", [{ type: "text", text: "启动后台任务" }]),
        storedMessage("assistant", [
          {
            type: "tool_call",
            id: "bg-call",
            name: "execute_command",
            arguments: JSON.stringify({
              command: task.command,
              run_in_background: true,
            }),
          },
        ]),
        storedMessage(
          "tool",
          [
            {
              type: "tool_result",
              call_id: "bg-call",
              content: "background envelope",
              is_error: false,
            },
          ],
          {
            details: {
              extra: {
                kind: "background_task",
                task_id: task.id,
                status: "running",
                command: task.command,
              },
            },
          },
        ),
        storedMessage("assistant", [{ type: "text", text: "已启动任务。" }]),
        storedMessage("user", [{ type: "text", text: "后台任务已完成" }], {
          details: {
            extra: {
              kind: "background_task_notification",
              tasks: [
                {
                  task_id: task.id,
                  command: task.command,
                  status: "completed",
                  exit_code: 0,
                },
              ],
            },
          },
        }),
        storedMessage("assistant", [
          { type: "text", text: "后台结果已就绪。" },
        ]),
      ],
    },
  });
  mounted = await mountApp();
  await mounted.router.push("/conversation/c1");
  await expect
    .element(page.getByPlaceholder(COMPOSER_SELECTOR))
    .toBeInTheDocument();
  await expect.poll(chatText).toContain("后台结果已就绪。");
}

it.each(["通知", "底栏"])(
  "%s定位展开目标工作过程后再滚动，同一任务可重复定位",
  async (entry) => {
    await bootTaskHistory();
    const toggles = page.getByTestId("chat-work-toggle");
    const targetToggle = toggles.nth(1);
    const card = page.getByTitle("查看输出文件");
    await expect
      .element(toggles.first())
      .toHaveAttribute("aria-expanded", "false");
    await expect
      .element(targetToggle)
      .toHaveAttribute("aria-expanded", "false");

    // 在真实滚动前确认目标已有可见布局，避免先滚动隐藏节点、再展开的竞态。
    const nativeScroll = Element.prototype.scrollIntoView;
    const scroll = vi.spyOn(Element.prototype, "scrollIntoView");
    let visibleWhenScrolled = false;
    scroll.mockImplementation(function (this: Element, options) {
      if (this.getAttribute("data-bg-task-id") === task.id)
        visibleWhenScrolled = this.checkVisibility();
      nativeScroll.call(this, options);
    });

    const locate = async () => {
      if (entry === "通知") {
        const button = page.getByTitle("定位到任务卡片");
        if (button.elements().length === 0)
          await page.getByRole("button", { name: /后台任务已完成/ }).click();
        await button.click();
      } else {
        await page.getByTitle("后台任务", { exact: true }).click();
        await page
          .getByRole("menuitem", { name: /echo background-result/ })
          .click();
      }
    };

    for (let attempt = 0; attempt < 2; attempt++) {
      visibleWhenScrolled = false;
      await locate();
      await expect.poll(() => visibleWhenScrolled).toBe(true);
      await expect
        .element(targetToggle)
        .toHaveAttribute("aria-expanded", "true");
      await expect.element(card).toBeVisible();
      await expect
        .element(toggles.first())
        .toHaveAttribute("aria-expanded", "false");
      expect(
        document
          .querySelector(`[data-bg-task-id="${task.id}"]`)
          ?.classList.contains("border-accent-6"),
      ).toBe(true);
      await targetToggle.click();
      await expect
        .element(targetToggle)
        .toHaveAttribute("aria-expanded", "false");
    }
  },
);
