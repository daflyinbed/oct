import { expect, it } from "vitest";
import { page } from "vitest/browser";
import { finishEvent, textDelta } from "./support/fixtures";
import { bootConversation, chatText, sendChatMessage } from "./support/harness";
import { getRequests, openGate } from "./support/mock";

// 文本流式往返:SSE 帧按恶意边界切块(多字节字符/JSON 中间)、keep-alive
// 注释行、gate 控制的两段式断言(流式中 / 流式后)。
it("streams assistant text across hostile chunk boundaries", async () => {
  const mounted = await bootConversation([
    { type: "events", events: [textDelta("Hello ")] },
    { type: "comment" },
    { type: "events", events: [textDelta("世界🌍")], slices: 6 },
    { type: "gate", name: "mid" },
    { type: "events", events: [textDelta(" done")], slices: 3 },
    { type: "events", events: [finishEvent()] },
  ]);

  await sendChatMessage("你好");

  // gate 之前的内容已渲染;流式光标与停止按钮在场(sending 状态可见)。
  await expect.poll(chatText).toContain("你好");
  await expect.poll(chatText).toContain("Hello 世界🌍");
  await expect.element(page.getByText("▌")).toBeInTheDocument();
  await expect.element(page.getByTitle("停止")).toBeInTheDocument();

  await openGate("mid");

  // gate 之后的内容到达,流经 finish + EOF 收尾:光标消失、发送按钮恢复。
  await expect.poll(chatText).toContain("Hello 世界🌍 done");
  await expect.element(page.getByText("▌")).not.toBeInTheDocument();
  await expect.element(page.getByTitle("发送（Enter）")).toBeInTheDocument();
  await expect
    .element(page.getByPlaceholder("Ask Oct to do something..."))
    .toBeEnabled();

  // 请求体带上自动选中的 provider spec(mock-prov 的 model-a)。
  const requests = await getRequests();
  const post = requests.find(
    (r) => r.method === "POST" && r.path === "/api/conversations/c1/messages",
  );
  expect(post?.body).toEqual({
    content: "你好",
    provider_spec: "mock-prov:model-a",
  });

  mounted.unmount();
});
