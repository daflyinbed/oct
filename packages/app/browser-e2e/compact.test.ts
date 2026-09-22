import { expect, it } from "vitest";
import { page } from "vitest/browser";
import {
  compactedEvent,
  compactingEvent,
  finishEvent,
  storedMessage,
  textDelta,
  usageEvent,
} from "./support/fixtures";
import { bootConversation, chatText, sendChatMessage } from "./support/harness";
import { getRequests } from "./support/mock";

// 上下文压缩：compacted 事件当场渲染可展开的分隔条、usage 事件驱动输入框
// 内的上下文圆环；/compact 斜杠命令走 REST 端点，标记行由历史重拉渲染。
it("compacted event renders an expandable divider and drives the context indicator", async () => {
  const mounted = await bootConversation([
    { type: "events", events: [compactingEvent("auto")] },
    {
      type: "events",
      events: [
        compactedEvent("E2E 摘要：修复了登录按钮并补齐测试", {
          compactedMessages: 7,
          estTokensBefore: 9000,
          estTokensAfter: 800,
        }),
        textDelta("继续干活"),
        usageEvent(120, 8, 2000),
        finishEvent(),
      ],
    },
  ]);

  await sendChatMessage("继续");

  // 分隔条出现在消息流里（compacted_messages=7），本轮文本紧随其后。
  await expect.poll(chatText).toContain("已压缩 7 条较早消息");
  await expect.poll(chatText).toContain("继续干活");

  // 输入框内的上下文圆环：usage 事件带窗口 → hover 弹层展示绝对值 +
  // 百分比（120/2000 = 6%）与压缩阈值（2000 × 85% = 1.7k）。
  const ring = page.getByTitle("上下文容量");
  await expect.element(ring).toBeInTheDocument();
  await ring.hover();
  await expect.element(page.getByText("120 / 2k（6%）")).toBeInTheDocument();
  await expect.element(page.getByText("85%（1.7k）")).toBeInTheDocument();

  // 展开分隔条看摘要，再收起。
  await page.getByTitle("展开摘要").click();
  await expect.poll(chatText).toContain("E2E 摘要：修复了登录按钮并补齐测试");
  await page.getByTitle("收起摘要").click();
  await expect
    .element(page.getByText("E2E 摘要：修复了登录按钮并补齐测试"))
    .not.toBeInTheDocument();

  mounted.unmount();
});

it("/compact command posts to the compact endpoint and renders the marker row", async () => {
  const mounted = await bootConversation([], {
    stored: [
      storedMessage("user", '[{"Text":"旧问题"}]'),
      storedMessage("assistant", '[{"Text":"旧回答"}]'),
    ],
  });

  await sendChatMessage("/compact");

  // 命令被拦截为 REST 调用，而不是一条聊天消息。
  const requests = await getRequests();
  expect(
    requests.find(
      (r) => r.method === "POST" && r.path === "/api/conversations/c1/compact",
    ),
  ).toBeTruthy();
  expect(
    requests.find(
      (r) =>
        r.method === "POST" &&
        r.path === "/api/conversations/c1/messages" &&
        (r.body as { content?: string })?.content === "/compact",
    ),
  ).toBeUndefined();

  // mock 落了标记行（compacted = 2 条旧消息），前端重拉后渲染分隔条。
  await expect.poll(chatText).toContain("已压缩 2 条较早消息");
  await page.getByTitle("展开摘要").click();
  await expect
    .poll(chatText)
    .toContain("E2E 压缩摘要：完成了登录修复并跑通测试");

  mounted.unmount();
});
