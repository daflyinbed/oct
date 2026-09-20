import { expect, it } from "vitest";
import { page } from "vitest/browser";
import { bootConversation, chatText, sendChatMessage } from "./support/harness";

// 发送失败路径:POST 直接返回 500(useChat 的 !res.ok 分支)——
// 错误文案落进助手气泡,输入框恢复可用。
it("send failure surfaces the error message and re-enables the composer", async () => {
  const mounted = await bootConversation([{ type: "status", code: 500 }]);

  await sendChatMessage("will fail");

  await expect.poll(chatText).toContain("Failed to send message.");
  await expect
    .element(page.getByPlaceholder("Ask Oct to do something..."))
    .toBeEnabled();
  await expect.element(page.getByTitle("发送（Enter）")).toBeInTheDocument();
  // 流式光标没有出现(从未进入流式状态)
  await expect.element(page.getByText("▌")).not.toBeInTheDocument();

  mounted.unmount();
});
