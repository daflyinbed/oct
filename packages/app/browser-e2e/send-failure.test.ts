import { afterEach, expect, it } from "vitest";
import { page } from "vitest/browser";
import { mountApp } from "./support/app";
import {
  conversationFixture,
  projectFixture,
  providerFixture,
  storedMessage,
  textDelta,
} from "./support/fixtures";
import { chatText, sendChatMessage } from "./support/harness";
import { resetMock, setScenario } from "./support/mock";
import type { MountedApp } from "./support/app";

let mounted: MountedApp | null = null;
afterEach(() => {
  mounted?.unmount();
  mounted = null;
});

// 发送失败路径:POST 直接返回 500 或 SSE error；历史带版本头——
// 提交的提示与错误文案留在气泡中,输入框恢复可用。
it.each(["http", "sse"] as const)(
  "send failure stays visible with versioned history and re-enables the composer (%s)",
  async (failure) => {
    await resetMock();
    await setScenario({
      projects: [projectFixture()],
      conversations: [conversationFixture()],
      providers: [providerFixture()],
      messages: { c1: [] },
      contextRevisions: { c1: 0 },
      scripts: {
        c1:
          failure === "http"
            ? [{ type: "status", code: 500 }]
            : [
                {
                  type: "events",
                  events: [
                    textDelta("partial reply"),
                    { type: "error", data: "provider disconnected" },
                  ],
                },
              ],
      },
      afterStreamHistory: {
        c1: {
          revision: 2,
          messages: [
            storedMessage("user", [{ type: "text", text: "will fail" }]),
            storedMessage("assistant", [
              { type: "text", text: "partial reply" },
            ]),
          ],
        },
      },
    });
    mounted = await mountApp();
    await mounted.router.push("/conversation/c1");
    await expect.element(page.getByTitle("分支当前会话")).toBeEnabled();

    await sendChatMessage("will fail");

    await expect
      .element(page.getByPlaceholder("Ask Oct to do something..."))
      .toBeEnabled();
    await expect.poll(chatText).toContain("will fail");
    await expect
      .poll(chatText)
      .toContain(
        failure === "http"
          ? "Failed to send message."
          : "Error: provider disconnected",
      );
    if (failure === "sse") {
      await expect.poll(chatText).toContain("partial reply");
    }
    await expect.element(page.getByTitle("发送（Enter）")).toBeInTheDocument();
    // 失败后不再保留流式光标。
    await expect.element(page.getByText("▌")).not.toBeInTheDocument();
    await expect.element(page.getByTitle("分支当前会话")).toBeDisabled();
  },
);
