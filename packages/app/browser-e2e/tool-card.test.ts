import { expect, it } from "vitest";
import { page } from "vitest/browser";
import {
  finishEvent,
  textDelta,
  toolCallDelta,
  toolCallStart,
  toolOutputDelta,
  toolResult,
} from "./support/fixtures";
import { bootConversation, chatText, sendChatMessage } from "./support/harness";
import { openGate } from "./support/mock";

// 工具卡片头按钮(execute_command 的 displayName 是"终端")
const TOOL_CARD_HEADER = /终端/;
const COMMAND_WITH_PATHS =
  "ls -l /proc/13249/cwd /proc/74702/cwd 2>/dev/null | head -40";

// 第二个文本段渲染在卡片之后;markdown 会吃掉段首空格
const TEXT_AFTER_TOOL_OUTPUT = /warn!\s*done/;

// 工具卡片全生命周期:generating(参数分片累积)→ running(标题 + 实时
// 输出)→ done(exit 徽标 + 返回内容)。三个阶段用 gate 分隔做确定性断言。
it("tool call card: generating → running with live output → done", async () => {
  const mounted = await bootConversation([
    { type: "events", events: [textDelta("Let me run ")] },
    // 参数 delta 按恶意边界切块;name 随首个 delta 到达 → 卡片显示"终端"
    {
      type: "events",
      events: [toolCallDelta("call_1", '{"comm', "execute_command")],
      slices: 4,
    },
    { type: "gate", name: "after-delta" },
    {
      type: "events",
      events: [
        toolCallStart(
          "call_1",
          "execute_command",
          COMMAND_WITH_PATHS,
          JSON.stringify({ command: COMMAND_WITH_PATHS }),
        ),
      ],
    },
    {
      type: "events",
      events: [toolOutputDelta("call_1", "stdout", "hello\n")],
    },
    { type: "events", events: [toolOutputDelta("call_1", "stderr", "warn!")] },
    { type: "gate", name: "before-result" },
    {
      type: "events",
      events: [
        toolResult("call_1", "hello\nwarn!", {
          exit_code: 0,
          wall_time_ms: 12,
        }),
      ],
    },
    { type: "events", events: [textDelta(" done")] },
    { type: "events", events: [finishEvent()] },
  ]);

  await sendChatMessage("run it");

  // generating:显示名已到(骨架标题),卡片处于折叠态
  await expect.poll(chatText).toContain("终端");
  await openGate("after-delta");

  // running:tool_call_start 带来标题;展开卡片检查终端块与实时输出
  await expect.poll(chatText).toContain(COMMAND_WITH_PATHS);
  const header = page.getByRole("button", { name: TOOL_CARD_HEADER });
  const title = (await header.element()).querySelector<HTMLElement>(
    ":scope > span[title]",
  )!;
  expect(title.textContent?.trim()).toBe(COMMAND_WITH_PATHS);
  expect(title.classList.contains("text-faint")).toBe(true);
  expect(title.children).toHaveLength(0);
  await header.click();
  await expect.poll(chatText).toContain("hello");
  await expect.poll(chatText).toContain("warn!");

  await openGate("before-result");

  // done:exit 徽标、流后追加的第二个文本段(text_delta 落在卡片之后,
  // 是独立的 text part,不与 "Let me run " 合并;markdown 渲染会吃掉
  // 段首空格)、光标消失(▋ 为 vue-stream-markdown 的 block caret)
  await expect.poll(chatText).toContain("exit 0");
  await expect.poll(chatText).toMatch(TEXT_AFTER_TOOL_OUTPUT);
  await expect.element(page.getByText("▋")).not.toBeInTheDocument();

  mounted.unmount();
});
