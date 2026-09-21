import { expect, it } from "vitest";
import { page } from "vitest/browser";
import {
  finishEvent,
  reasoningDelta,
  textDelta,
  toolCallDelta,
  toolCallStart,
  toolResult,
} from "./support/fixtures";
import { bootConversation, chatText, sendChatMessage } from "./support/harness";
import { openGate } from "./support/mock";

// 思考卡片头按钮
const THINKING_CARD_HEADER = /思考/;

// 思考卡片:流式期间展开跟随,首个非思考事件结束后折叠为带时长的
// 单行摘要,点击可再展开。
it("thinking card streams open, collapses with duration, reopens on click", async () => {
  const mounted = await bootConversation([
    {
      type: "events",
      events: [
        reasoningDelta("先看看目录结构"),
        reasoningDelta("，再决定读哪个文件"),
      ],
    },
    { type: "gate", name: "mid" },
    { type: "events", events: [textDelta("我来帮你看看。")] },
    { type: "events", events: [finishEvent()] },
  ]);

  await sendChatMessage("看看这个项目");

  // gate 之前:思考仍在流式,正文展开可见,还没有时长摘要。
  await expect.poll(chatText).toContain("先看看目录结构，再决定读哪个文件");
  await expect
    .element(page.getByRole("button", { name: THINKING_CARD_HEADER }))
    .toHaveAttribute("aria-expanded", "true");
  await expect.poll(chatText).not.toContain("持续了几秒");

  await openGate("mid");

  // 结束后:卡片折叠,正文不再渲染,摘要带时长。
  await expect.poll(chatText).toContain("思考");
  await expect.poll(chatText).toContain("持续了几秒");
  await expect.poll(chatText).not.toContain("先看看目录结构");
  await expect
    .element(page.getByRole("button", { name: THINKING_CARD_HEADER }))
    .toHaveAttribute("aria-expanded", "false");

  // 点击头行重新展开正文。
  await page.getByRole("button", { name: THINKING_CARD_HEADER }).click();
  await expect.poll(chatText).toContain("先看看目录结构，再决定读哪个文件");

  mounted.unmount();
});

it("tool loop with reasoning shows one collapsible thinking block per round", async () => {
  const mounted = await bootConversation([
    {
      type: "events",
      events: [
        reasoningDelta("第一轮思考"),
        toolCallDelta("call-1", "{}", "list_dir"),
        toolCallStart("call-1", "list_dir", ".", "{}"),
        toolResult("call-1", "a.txt\nb.txt"),
        reasoningDelta("第二轮思考"),
        textDelta("目录里有 a.txt 和 b.txt。"),
        finishEvent(),
      ],
    },
  ]);

  await sendChatMessage("列出文件");

  // 两轮各一块,结束后都折叠为摘要。
  await expect.poll(chatText).toContain("持续了几秒");
  await expect.poll(chatText).toContain("目录里有 a.txt 和 b.txt。");
  const headers = page.getByRole("button", { name: THINKING_CARD_HEADER });
  await expect.poll(async () => (await headers.elements()).length).toBe(2);

  // 两块展开后各自的正文可见(第一块的正文此前被折叠隐藏)。
  const els = await headers.elements();
  for (const el of els) el.click();
  await expect.poll(chatText).toContain("第一轮思考");
  await expect.poll(chatText).toContain("第二轮思考");

  mounted.unmount();
});
