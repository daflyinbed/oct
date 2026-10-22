import { afterEach, expect, it } from "vitest";
import { page } from "vitest/browser";
import { useChat } from "@/composables/useChat";
import {
  cancelledEvent,
  finishEvent,
  reasoningDelta,
  storedMessage,
  textDelta,
} from "./support/fixtures";
import { bootConversation, chatText, sendChatMessage } from "./support/harness";
import { openGate } from "./support/mock";
import type { MountedApp } from "./support/app";

let mounted: MountedApp | null = null;
afterEach(() => {
  mounted?.unmount();
  mounted = null;
  useChat().clearMessages();
});

function workText() {
  return page.getByTestId("chat-work-toggle").element().textContent?.trim();
}

it("calculates from DTO timestamps after completion, history replacement and reopening", async () => {
  // Both timestamps are server facts; their difference is independent of the browser clock.
  const stored = [
    storedMessage("user", [{ type: "text", text: "检查" }], {
      id: "u",
      started_at: "2026-10-21T00:00:00Z",
    }),
    storedMessage(
      "assistant",
      [
        { type: "reasoning", text: "检查过程" },
        { type: "text", text: "完成" },
      ],
      { id: "a", finished_at: "2026-10-21T01:02:03Z" },
    ),
  ];
  mounted = await bootConversation(
    [
      { type: "events", events: [reasoningDelta("检查过程")] },
      { type: "gate", name: "finish" },
      { type: "events", events: [textDelta("完成"), finishEvent()] },
    ],
    { afterStreamHistory: { c1: { messages: stored, revision: 2 } } },
  );
  await sendChatMessage("检查");
  await expect.poll(workText).toMatch(/^已工作0分\d+秒$/);
  await openGate("finish");
  await expect.poll(workText).toBe("已工作1小时2分3秒");
  await expect
    .poll(() => document.querySelector('[data-message-id="u"]'))
    .not.toBeNull();
  await mounted.router.push("/");
  await expect.poll(chatText).toContain("Start a conversation");
  await mounted.router.push("/conversation/c1");
  await expect.poll(workText).toBe("已工作1小时2分3秒");
  await expect
    .element(page.getByTestId("chat-work-toggle"))
    .toHaveAttribute("aria-expanded", "false");
});

it("shows persisted days in cold history without a live stream", async () => {
  mounted = await bootConversation([], {
    stored: [
      storedMessage("user", [{ type: "text", text: "长任务" }], {
        id: "u",
        started_at: "2026-10-20T00:00:00Z",
      }),
      storedMessage(
        "assistant",
        [
          { type: "reasoning", text: "工作过程" },
          { type: "text", text: "完成" },
        ],
        { finished_at: "2026-10-21T02:03:04Z" },
      ),
    ],
  });
  await expect.poll(workText).toBe("已工作1天2小时3分4秒");
});

it.each([
  [cancelledEvent(), "已停止"],
  [{ type: "error", data: "连接中断" }, "执行失败"],
] as const)("does not display elapsed time after %j", async (event, label) => {
  mounted = await bootConversation([
    {
      type: "events",
      events: [reasoningDelta("检查过程"), textDelta("未完成的回复"), event],
    },
  ]);
  await sendChatMessage("检查");
  await expect.poll(workText).toBe(label);
  await expect
    .element(page.getByTestId("chat-work-toggle"))
    .toHaveAttribute("aria-expanded", "true");
});
