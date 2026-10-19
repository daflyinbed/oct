import { afterEach, expect, it } from "vitest";
import { page, userEvent } from "vitest/browser";
import {
  finishEvent,
  reasoningDelta,
  storedMessage,
  textDelta,
} from "./support/fixtures";
import {
  bootConversation,
  chatText,
  COMPOSER_SELECTOR,
  sendChatMessage,
} from "./support/harness";
import { openGate } from "./support/mock";
import type { MountedApp } from "./support/app";

let mounted: MountedApp | null = null;
afterEach(() => {
  mounted?.unmount();
  mounted = null;
});

function indicators() {
  return page
    .getByRole("navigation", { name: "用户消息导航" })
    .getByRole("button");
}

function lineStyle(index: number) {
  const line = indicators().nth(index).element().querySelector("span")!;
  const style = getComputedStyle(line);
  return {
    width: Number.parseFloat(style.width),
    color: style.backgroundColor,
  };
}

function previewText() {
  return page.getByRole("tooltip").query()?.textContent ?? "";
}

it("previews each user turn with its first reply and highlights the latest message", async () => {
  mounted = await bootConversation([], {
    stored: [
      storedMessage("user", [{ type: "text", text: "UnityMain 是什么？" }]),
      storedMessage("assistant", [{ type: "reasoning", text: "私有思考" }]),
      storedMessage("assistant", [
        { type: "text", text: "我先说明播放器的入口。" },
      ]),
      storedMessage("assistant", [{ type: "text", text: "后来补充的结论。" }]),
      storedMessage("user", [{ type: "text", text: "尚未回答的问题" }]),
      storedMessage(
        "user",
        [{ type: "text", text: "<summary>旧摘要</summary>" }],
        {
          details: {
            kind: "compaction",
            compaction: { triggered_by: "manual", compacted_message_count: 2 },
          },
        },
      ),
      storedMessage("user", [{ type: "text", text: "后台任务通知" }], {
        details: { extra: { kind: "background_task_notification", tasks: [] } },
      }),
      storedMessage("user", [{ type: "text", text: "模型专用附件快照" }], {
        user_content: {
          body: "",
          attachments: [
            {
              id: "notes",
              filename: "notes.txt",
              mime_type: "text/plain",
              size_bytes: 5,
            },
          ],
        },
      }),
    ],
  });

  await expect.poll(() => indicators().elements().length).toBe(3);
  await page.getByPlaceholder(COMPOSER_SELECTOR).hover();
  expect([0, 1, 2].map((index) => lineStyle(index).width)).toEqual([
    16, 16, 16,
  ]);
  expect(indicators().nth(0).element().hasAttribute("aria-current")).toBe(
    false,
  );
  expect(indicators().last().element().getAttribute("aria-current")).toBe(
    "true",
  );
  expect(lineStyle(0).color).not.toBe(lineStyle(2).color);

  await indicators().first().hover();
  const preview = page.getByRole("tooltip");
  await expect.poll(previewText).toContain("UnityMain 是什么？");
  await expect.poll(previewText).toContain("我先说明播放器的入口。");
  await expect.poll(previewText).not.toContain("私有思考");
  await expect.poll(previewText).not.toContain("后来补充的结论");
  await expect.poll(() => lineStyle(0).width).toBe(32);
  expect(lineStyle(0).color).toBe(lineStyle(2).color);
  const lines = preview.element().querySelectorAll("p");
  expect(lines[0]?.textContent).toContain("UnityMain");
  expect(lines[1]?.textContent).toContain("播放器的入口");

  await indicators().nth(1).hover();
  await expect.poll(previewText).toContain("尚未回答的问题");
  await expect.poll(previewText).toContain("暂无回复");
  await expect.poll(previewText).not.toContain("播放器的入口");
  await expect.poll(() => lineStyle(0).width).toBe(16);

  await page.getByPlaceholder(COMPOSER_SELECTOR).hover();
  indicators().last().element().focus();
  await expect.poll(previewText).toContain("notes.txt");
  await expect.poll(previewText).not.toContain("模型专用附件快照");
  await userEvent.keyboard("{Escape}");
  await expect.element(preview).not.toBeInTheDocument();

  await mounted.router.push("/");
  await expect
    .element(page.getByRole("navigation", { name: "用户消息导航" }))
    .not.toBeInTheDocument();
  await expect.element(preview).not.toBeInTheDocument();
});

it("updates streaming previews and preserves the position when jumping to an earlier question", async () => {
  const stored = Array.from({ length: 6 }, (_, index) => [
    storedMessage("user", [{ type: "text", text: `历史问题 ${index + 1}` }]),
    storedMessage("assistant", [
      {
        type: "text",
        text: Array.from(
          { length: 12 },
          (_, line) => `回答 ${index + 1} 的第 ${line + 1} 段。`,
        ).join("\n\n"),
      },
    ]),
  ]).flat();
  mounted = await bootConversation(
    [
      { type: "events", events: [reasoningDelta("正在思考")] },
      { type: "gate", name: "thinking" },
      { type: "events", events: [textDelta("我先检查当前实现。")] },
      { type: "gate", name: "first-answer" },
      {
        type: "events",
        events: [textDelta("后续内容继续到达。"), finishEvent()],
      },
    ],
    { stored },
  );

  try {
    await expect.poll(() => indicators().elements().length).toBe(6);
    await sendChatMessage("继续检查");
    await expect.poll(() => indicators().elements().length).toBe(7);
    const scroll = document.querySelector<HTMLElement>(
      "main .workspace-panel-body",
    )!;
    await expect.poll(() => scroll.scrollTop).toBeGreaterThan(0);

    await indicators().last().hover();
    await expect.poll(previewText).toContain("正在回复…");
    await openGate("thinking");
    await expect.poll(previewText).toContain("我先检查当前实现。");

    // 在平滑定位尚未结束时继续流，不能让新增文本把视图拉回底部。
    await indicators().first().click();
    await openGate("first-answer");
    await expect.poll(chatText).toContain("后续内容继续到达。");
    await expect.poll(() => scroll.scrollTop, { timeout: 3000 }).toBe(0);
    await expect.element(page.getByTitle("发送（Enter）")).toBeInTheDocument();
    expect(indicators().last().element().getAttribute("aria-current")).toBe(
      "true",
    );
  } finally {
    await openGate("thinking");
    await openGate("first-answer");
  }
});

it("keeps the latest indicator reachable when a long conversation overflows the rail", async () => {
  mounted = await bootConversation([], {
    stored: Array.from({ length: 60 }, (_, index) =>
      storedMessage("user", [{ type: "text", text: `问题 ${index + 1}` }]),
    ),
  });
  await expect.poll(() => indicators().elements().length).toBe(60);
  const navigation = page
    .getByRole("navigation", { name: "用户消息导航" })
    .element();
  await expect.poll(() => navigation.scrollTop).toBeGreaterThan(0);
  await expect.element(indicators().last()).toBeVisible();
  expect(navigation.scrollHeight).toBeGreaterThan(navigation.clientHeight);

  navigation.scrollTop = 0;
  await indicators().first().hover();
  await expect.poll(previewText).toContain("问题 1");
  await indicators().first().click();
  const scroll = document.querySelector<HTMLElement>(
    "main .workspace-panel-body",
  )!;
  const firstMessage = scroll.querySelector<HTMLElement>("[data-message-id]")!;
  await expect
    .poll(
      () =>
        firstMessage.getBoundingClientRect().top -
        scroll.getBoundingClientRect().top,
      { timeout: 3000 },
    )
    .toBeCloseTo(18, 0);
});
