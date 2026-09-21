import { expect, it } from "vitest";
import { page } from "vitest/browser";
import { finishEvent, textDelta, titleUpdated } from "./support/fixtures";
import { bootConversation, chatText, sendChatMessage } from "./support/harness";
import { openGate } from "./support/mock";

// title_updated 是会话元数据事件：SSE 消费层拦截后派发到 useProjects 的
// 会话列表，侧栏条目与标签页标题即时更新，消息流内不出现标题文本。
it("title_updated updates the sidebar item and the tab title, not the message stream", async () => {
  const mounted = await bootConversation([
    { type: "events", events: [textDelta("working")] },
    { type: "gate", name: "mid" },
    { type: "events", events: [titleUpdated("修复登录按钮")] },
    { type: "events", events: [finishEvent()] },
  ]);

  await sendChatMessage("登录按钮坏了");
  await openGate("mid");

  // 标题事件落地：侧栏会话条目与标签页（同名 button）都换上新标题。
  await expect
    .poll(async () => {
      const items = await page
        .getByRole("button", { name: "修复登录按钮" })
        .elements();
      return items.length;
    })
    .toBe(2);

  // 标题不出现在消息流里。
  expect(await chatText()).not.toContain("修复登录按钮");

  mounted.unmount();
});
