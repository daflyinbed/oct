import { afterEach, expect, it, vi } from "vitest";
import { page, userEvent } from "vitest/browser";
import {
  cancelledEvent,
  finishEvent,
  reasoningDelta,
  storedMessage,
  textDelta,
  toolCallStart,
  toolResult,
} from "./support/fixtures";
import { bootConversation, chatText, sendChatMessage } from "./support/harness";
import { openGate } from "./support/mock";
import type { MountedApp } from "./support/app";
import type { components } from "@/api/schema";

let mounted: MountedApp | null = null;
afterEach(() => {
  mounted?.unmount();
  mounted = null;
  vi.restoreAllMocks();
});

const workToggle = () => page.getByTestId("chat-work-toggle");
function command() {
  return toolCallStart(
    "call",
    "execute_command",
    "检查构建",
    '{"command":"pnpm build"}',
  );
}

it("folds completed work around the final reply and preserves nested card choices", async () => {
  mounted = await bootConversation([
    {
      type: "events",
      events: [
        reasoningDelta("检查现有实现"),
        textDelta("先运行构建。"),
        command(),
        toolResult("call", "构建通过"),
      ],
    },
    { type: "gate", name: "reply" },
    {
      type: "events",
      events: [
        reasoningDelta("确认修复结果"),
        textDelta("已完成修复，构建通过。"),
        finishEvent(),
      ],
    },
  ]);
  await sendChatMessage("修复这个问题");
  await expect.poll(chatText).toContain("先运行构建。");
  await expect.element(workToggle()).toHaveAttribute("aria-expanded", "true");

  // 内层思考与工具各自保留用户选择；外层支持键盘操作。
  const thinking = page.getByRole("button", { name: /思考/ });
  await thinking.click();
  await page.getByRole("button", { name: /终端/ }).click();
  await expect.poll(chatText).toContain("构建通过");
  workToggle().element().focus();
  await userEvent.keyboard("{Enter}");
  await expect.element(workToggle()).toHaveAttribute("aria-expanded", "false");
  await expect.poll(chatText).not.toContain("先运行构建。");
  await userEvent.keyboard("{Enter}");
  await expect.element(workToggle()).toHaveAttribute("aria-expanded", "true");
  await expect.element(thinking).toHaveAttribute("aria-expanded", "true");

  await openGate("reply");
  await expect.poll(chatText).toContain("已完成修复，构建通过。");
  await expect.element(workToggle()).toHaveAttribute("aria-expanded", "false");
  await expect
    .poll(() => workToggle().element().textContent?.trim())
    .toMatch(/^已工作\d+分\d+秒$/);
  await expect.poll(chatText).not.toContain("先运行构建。");
  const answer = document.querySelector<HTMLElement>("[data-chat-answer]")!;
  expect(answer.closest("[data-chat-work-history]")).toBeNull();
  expect(document.querySelectorAll('button[title="复制"]')).toHaveLength(1);

  const copy = vi.spyOn(navigator.clipboard, "writeText").mockResolvedValue();
  await page.getByTitle("复制", { exact: true }).click();
  expect(copy).toHaveBeenCalledWith("已完成修复，构建通过。");

  await workToggle().click();
  await expect.poll(chatText).toContain("先运行构建。");
  await expect.poll(chatText).toContain("检查现有实现");
  await expect
    .element(page.getByRole("button", { name: /思考/ }).first())
    .toHaveAttribute("aria-expanded", "true");
  await expect.poll(chatText).toContain("构建通过");
});

it("collapses cold history across tool batches and retains the authorized fork cuts", async () => {
  mounted = await bootConversation([], {
    contextRevision: 4,
    stored: [
      storedMessage("user", [{ type: "text", text: "检查构建" }], {
        id: "u",
        fork_cut: { kind: "message", message_id: "u", side: "before" },
      }),
      storedMessage(
        "assistant",
        [
          { type: "text", text: "开始检查。" },
          {
            type: "tool_call",
            id: "call",
            name: "execute_command",
            arguments: '{"command":"pnpm build"}',
          },
        ],
        { id: "step" },
      ),
      storedMessage(
        "tool",
        [
          {
            type: "tool_result",
            call_id: "call",
            content: "ok",
            is_error: false,
          },
        ],
        {
          id: "result",
          fork_cut: { kind: "message", message_id: "result", side: "after" },
        },
      ),
      storedMessage(
        "assistant",
        [
          { type: "reasoning", text: "整理结论" },
          { type: "text", text: "构建已通过。" },
        ],
        {
          id: "reply",
          fork_cut: { kind: "message", message_id: "reply", side: "after" },
        },
      ),
    ],
  });
  await expect.poll(chatText).toContain("构建已通过。");
  await expect.element(workToggle()).toHaveAttribute("aria-expanded", "false");
  await expect.element(workToggle()).toHaveTextContent("已工作（耗时未知）");
  await expect.element(page.getByTitle("从这条回复后分支")).toBeVisible();
  await expect.poll(chatText).not.toContain("开始检查。");
  await workToggle().click();
  await expect.poll(chatText).toContain("开始检查。");
  await expect.element(page.getByTitle("从这一步后分支")).toBeInTheDocument();
  expect(
    document.querySelector(
      '[data-message-id="step"] [data-fork-marker="step"]',
    ),
  ).not.toBeNull();
  expect(document.querySelectorAll('button[title="复制"]')).toHaveLength(1);
});

it.each([
  ["cancelled", cancelledEvent(), "已停止"],
  ["error", { type: "error", data: "连接中断" }, "执行失败"],
] satisfies [string, components["schemas"]["AgentEvent"], string][])(
  "forces %s work open even after the user collapsed the running process",
  async (_, event, label) => {
    mounted = await bootConversation([
      {
        type: "events",
        events: [
          command(),
          toolResult("call", "ok"),
          textDelta("尚未完成的回复"),
        ],
      },
      { type: "gate", name: "stop" },
      { type: "events", events: [event] },
    ]);
    await sendChatMessage("检查");
    await expect.poll(chatText).toContain("尚未完成的回复");
    await workToggle().click();
    await expect
      .element(workToggle())
      .toHaveAttribute("aria-expanded", "false");
    await openGate("stop");
    await expect.poll(chatText).toContain(label);
    await expect.element(workToggle()).toHaveAttribute("aria-expanded", "true");
    await expect.poll(chatText).toContain("尚未完成的回复");
    expect(document.querySelector("[data-chat-answer]")).toBeNull();
    await expect.element(page.getByTitle("发送（Enter）")).toBeVisible();
  },
);

it("keeps work with no final body expanded after finish", async () => {
  mounted = await bootConversation([
    {
      type: "events",
      events: [
        textDelta("准备检查。"),
        command(),
        toolResult("call", "ok"),
        finishEvent(),
      ],
    },
  ]);
  await sendChatMessage("检查");
  await expect.element(page.getByTitle("发送（Enter）")).toBeVisible();
  await expect.element(workToggle()).toHaveAttribute("aria-expanded", "true");
  await expect.poll(chatText).toContain("准备检查。");
  expect(document.querySelector("[data-chat-answer]")).toBeNull();
});

it("converges a streamed turn to persisted history without duplicating the answer", async () => {
  mounted = await bootConversation(
    [
      {
        type: "events",
        events: [
          textDelta("先检查。"),
          command(),
          toolResult("call", "ok"),
          textDelta("最终结论。"),
          finishEvent(),
        ],
      },
    ],
    {
      afterStreamHistory: {
        c1: {
          revision: 4,
          messages: [
            storedMessage("user", [{ type: "text", text: "检查" }], {
              id: "persisted-user",
            }),
            storedMessage(
              "assistant",
              [
                { type: "text", text: "先检查。" },
                {
                  type: "tool_call",
                  id: "call",
                  name: "execute_command",
                  arguments: '{"command":"pnpm build"}',
                },
              ],
              { id: "step" },
            ),
            storedMessage("tool", [
              {
                type: "tool_result",
                call_id: "call",
                content: "ok",
                is_error: false,
              },
            ]),
            storedMessage("assistant", [{ type: "text", text: "最终结论。" }], {
              id: "reply",
            }),
          ],
        },
      },
    },
  );
  await sendChatMessage("检查");
  await expect
    .poll(() => document.querySelector('[data-message-id="reply"]'))
    .not.toBeNull();
  await expect.element(workToggle()).toHaveAttribute("aria-expanded", "false");
  expect(document.querySelectorAll("[data-chat-answer]")).toHaveLength(1);
  expect(document.querySelectorAll("[data-chat-work]")).toHaveLength(1);
  await expect.poll(chatText).toContain("最终结论。");
  await expect.poll(chatText).not.toContain("先检查。");
});

it("retains stopped work when history includes a task notification inserted before its prompt", async () => {
  mounted = await bootConversation(
    [
      {
        type: "events",
        events: [
          command(),
          toolResult("call", "ok"),
          reasoningDelta("正在整理结果"),
          textDelta("来不及写完的回复"),
          cancelledEvent(),
        ],
      },
    ],
    {
      afterStreamHistory: {
        c1: {
          revision: 4,
          requestMessageId: "u",
          messages: [
            storedMessage("user", [{ type: "text", text: "后台任务已完成" }], {
              id: "notification",
              details: {
                extra: { kind: "background_task_notification", tasks: [] },
              },
            }),
            storedMessage("user", [{ type: "text", text: "检查" }], {
              id: "u",
            }),
            storedMessage("assistant", [
              {
                type: "tool_call",
                id: "call",
                name: "execute_command",
                arguments: '{"command":"pnpm build"}',
              },
            ]),
            storedMessage("tool", [
              {
                type: "tool_result",
                call_id: "call",
                content: "ok",
                is_error: false,
              },
            ]),
          ],
        },
      },
    },
  );
  await sendChatMessage("检查");
  await expect.element(page.getByTitle("发送（Enter）")).toBeVisible();
  await expect
    .poll(() => document.querySelector('[data-message-id="u"]'))
    .not.toBeNull();
  await expect.poll(chatText).toContain("已停止");
  await expect.poll(chatText).toContain("来不及写完的回复");
  await expect.element(workToggle()).toHaveAttribute("aria-expanded", "true");
  expect(page.getByRole("button", { name: /终端/ }).elements()).toHaveLength(1);
  expect(document.querySelector("[data-chat-answer]")).toBeNull();
});
