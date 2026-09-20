import { expect, it } from "vitest";
import { page } from "vitest/browser";
import { toolCallStart, toolOutputDelta } from "./support/fixtures";
import { bootConversation, chatText, sendChatMessage } from "./support/harness";
import { getRequests } from "./support/mock";

// 工具卡片头按钮(execute_command 的 displayName 是"终端")
const TOOL_CARD_HEADER = /终端/;

// 取消路径:剧本在工具运行中挂起流(hang),点击"停止"→ 前端调用
// cancel 接口 → mock 向打开的 SSE 注入 cancelled 事件并结束流 →
// running 卡片落定为错误态,sending 归位。
it("stop button cancels the hanging run and settles the card as errored", async () => {
  const mounted = await bootConversation([
    {
      type: "events",
      events: [
        toolCallStart(
          "call_1",
          "execute_command",
          "sleep 60",
          '{"command":"sleep 60"}',
        ),
      ],
    },
    {
      type: "events",
      events: [toolOutputDelta("call_1", "stdout", "partial")],
    },
    { type: "hang" },
  ]);

  await sendChatMessage("run");

  // 实时输出只在展开的卡片里渲染:先展开,再等 running 中的 live output
  await expect.poll(chatText).toContain("sleep 60");
  await page.getByRole("button", { name: TOOL_CARD_HEADER }).click();
  await expect.poll(chatText).toContain("partial");
  await expect.element(page.getByTitle("停止")).toBeInTheDocument();

  await page.getByTitle("停止").click();

  // cancelled 事件 + 流结束:光标消失、发送按钮恢复、卡片转为错误色
  await expect.element(page.getByText("▌")).not.toBeInTheDocument();
  await expect.element(page.getByTitle("发送（Enter）")).toBeInTheDocument();
  await expect
    .poll(async () => {
      const el = await page.getByText("终端").element();
      return el.className;
    })
    .toContain("text-danger-10");

  const requests = await getRequests();
  expect(
    requests.some(
      (r) => r.method === "POST" && r.path === "/api/conversations/c1/cancel",
    ),
  ).toBe(true);

  mounted.unmount();
});
