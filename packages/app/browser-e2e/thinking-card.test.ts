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
const INITIAL_REASONING = "先看看目录结构。";
const REASONING_CONTINUATION =
  "先看看目录结构，再决定读哪个文件；然后检查相关组件和状态更新路径，确认流式内容在折叠行里持续向右跟随最新文本。".repeat(
    4,
  );
const COMPLETE_REASONING = INITIAL_REASONING + REASONING_CONTINUATION;

// 思考卡片默认折叠，流式文本在头行横向跟随；用户主动展开后保持展开。
it("thinking card streams in its collapsed row and follows the latest text", async () => {
  const mounted = await bootConversation([
    {
      type: "events",
      events: [reasoningDelta(INITIAL_REASONING)],
    },
    { type: "gate", name: "more-reasoning" },
    { type: "events", events: [reasoningDelta(REASONING_CONTINUATION)] },
    { type: "gate", name: "mid" },
    { type: "events", events: [textDelta("我来帮你看看。")] },
    { type: "events", events: [finishEvent()] },
  ]);

  await sendChatMessage("看看这个项目");

  // gate 之前：卡片保持折叠，流式正文只在单行预览里显示并滚到末尾。
  const header = page.getByRole("button", { name: THINKING_CARD_HEADER });
  await expect.element(header).toHaveAttribute("aria-expanded", "false");
  await expect.poll(chatText).not.toContain("持续了几秒");
  const preview = header.getByTestId("reasoning-preview");
  const previewElement = await preview.element();
  await expect
    .poll(() => previewElement.textContent)
    .toContain(INITIAL_REASONING);

  // 新增 delta 后仍是同一行，并自动跟随到最新文本的最右端。
  await openGate("more-reasoning");
  await expect
    .poll(() => previewElement.textContent)
    .toContain(COMPLETE_REASONING);
  await expect
    .poll(() => previewElement.scrollWidth > previewElement.clientWidth)
    .toBe(true);
  await expect
    .poll(
      () =>
        Math.abs(
          previewElement.scrollWidth -
            previewElement.clientWidth -
            previewElement.scrollLeft,
        ) < 2,
    )
    .toBe(true);
  expect(getComputedStyle(previewElement).whiteSpace).toBe("nowrap");
  expect(getComputedStyle(previewElement).overflowX).toBe("auto");
  expect(document.querySelector("[data-reasoning-body]")).toBeNull();

  // 用户在流式期间主动展开后，显示完整正文。
  await header.click();
  await expect.element(header).toHaveAttribute("aria-expanded", "true");
  await expect.poll(chatText).toContain(COMPLETE_REASONING);
  expect(document.querySelector("[data-reasoning-preview]")).toBeNull();
  expect(document.querySelector("[data-reasoning-body]")).not.toBeNull();

  await openGate("mid");

  // 流式结束后保留用户的展开选择，并显示耗时。
  await expect.poll(chatText).toContain("思考");
  await expect.poll(chatText).toContain("持续了几秒");
  await expect.element(header).toHaveAttribute("aria-expanded", "true");
  await expect.poll(chatText).toContain(COMPLETE_REASONING);

  // 再次点击恢复折叠，结束后的单行只保留摘要。
  await header.click();
  await expect.element(header).toHaveAttribute("aria-expanded", "false");
  await expect.poll(chatText).not.toContain(COMPLETE_REASONING);

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
  for (const el of await headers.elements()) {
    expect(el.getAttribute("aria-expanded")).toBe("false");
  }

  // 两块展开后各自的正文可见(第一块的正文此前被折叠隐藏)。
  const els = await headers.elements();
  for (const el of els) el.click();
  await expect.poll(chatText).toContain("第一轮思考");
  await expect.poll(chatText).toContain("第二轮思考");

  mounted.unmount();
});
