import { expect, it } from "vitest";
import { nextTick } from "vue";
import {
  conversationFixture,
  finishEvent,
  projectFixture,
  providerFixture,
  reasoningDelta,
  storedMessage,
  textDelta,
  toolCallStart,
  toolOutputDelta,
  toolResult,
} from "./support/fixtures";
import { bootConversation, chatText, sendChatMessage } from "./support/harness";
import { setScenario } from "./support/mock";

// 历史重放一致性:live 流渲染完的会话,重进后由 parts_json(StoredMessage
// fixture,与剧本事件一一对应)重建,渲染文本必须与 live 结束态完全一致。
// 这条 pin 住 applyStreamEvent(live)与 storedToDisplayList(重放)两条
// 解析路径的等价性。
it("history replay renders identically to the finished live stream", async () => {
  const mounted = await bootConversation([
    { type: "events", events: [reasoningDelta("让我想想")] },
    { type: "events", events: [textDelta("Done.")] },
    {
      type: "events",
      events: [
        toolCallStart("call_1", "read_file", "a.txt", '{"path":"a.txt"}'),
      ],
    },
    {
      type: "events",
      events: [toolOutputDelta("call_1", "stdout", "file body\n")],
    },
    {
      type: "events",
      events: [
        toolResult("call_1", "file body", {
          title: "a.txt",
          shown_lines: 10,
          total_lines: 10,
        }),
      ],
    },
    { type: "events", events: [finishEvent()] },
  ]);

  await sendChatMessage("hi");
  // live 结束态:思考摘要(已折叠) + 文本 + 卡片头(读取文件 a.txt),光标消失
  await expect.poll(chatText).toContain("持续了几秒");
  await expect.poll(chatText).toContain("Done.");
  await expect.poll(chatText).toContain("读取文件");
  await expect.poll(chatText).toContain("a.txt");
  await expect.poll(chatText).not.toContain("▌");
  const liveText = await chatText();

  // 会话持久化后的形状(mock 直接给出与剧本对应的 StoredMessage)
  await setScenario({
    projects: [projectFixture()],
    conversations: [conversationFixture()],
    providers: [providerFixture()],
    messages: {
      c1: [
        storedMessage("user", JSON.stringify([{ Text: "hi" }])),
        storedMessage(
          "assistant",
          JSON.stringify([
            { Reasoning: "让我想想" },
            { Text: "Done." },
            {
              ToolCall: {
                id: "call_1",
                name: "read_file",
                arguments: '{"path":"a.txt"}',
              },
            },
          ]),
          // 与 live 结束态渲染一致:live 时长是几毫秒 →「几秒」,
          // 这里存 5000ms 同样落在模糊文案区间。
          { details_json: JSON.stringify({ reasoning_duration_ms: 5000 }) },
        ),
        storedMessage(
          "tool",
          JSON.stringify([
            {
              ToolResult: {
                call_id: "call_1",
                content: "file body",
                is_error: false,
              },
            },
          ]),
          {
            details_json: JSON.stringify({
              title: "a.txt",
              shown_lines: 10,
              total_lines: 10,
            }),
          },
        ),
      ],
    },
  });

  // 离开(回到 index 页)再回来,route watch 触发 fetchMessages 重建列表。
  // boot 时的拉取与重进之间隔着整段 live 流,useChat 的 200ms 去重窗口
  // 早已过去,重进必定重新拉取。
  await mounted.router.push("/");
  await nextTick();
  await expect.poll(chatText).toContain("Start a conversation");
  await mounted.router.push("/conversation/c1");
  await nextTick();

  await expect.poll(chatText).toBe(liveText);

  mounted.unmount();
});
