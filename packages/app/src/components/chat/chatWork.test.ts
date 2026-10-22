import { describe, expect, it } from "vitest";
import { newToolCard, storedToDisplayList } from "@/composables/chat/display";
import { buildChatRenderUnits, formatWorkDuration } from "./chatWork";
import type { ChatWork } from "./chatWork";
import type {
  ApiMessage,
  DisplayMessage,
  DisplayPart,
  DisplayWork,
} from "@/composables/chat/display";

const text = (value: string): DisplayPart => ({ kind: "text", text: value });
const reasoning: DisplayPart = {
  kind: "reasoning",
  text: "检查实现",
  startedAt: null,
  durationMs: null,
  ended: true,
};
const tool: DisplayPart = {
  ...newToolCard("call", "read"),
  status: "done",
  content: "file",
};
const user: DisplayMessage = {
  id: "user",
  role: "user",
  parts: [text("修复问题")],
};

function assistant(
  id: string,
  parts: DisplayPart[],
  work?: DisplayWork,
): DisplayMessage {
  return { id, role: "assistant", parts, work };
}

function lastWork(messages: DisplayMessage[], options = {}): ChatWork {
  const unit = buildChatRenderUnits(messages, options).at(-1)!;
  expect(unit.kind).toBe("work");
  return unit as ChatWork;
}

describe("chat work segmentation", () => {
  it("uses the same answer boundary for a live message and persisted tool rounds", () => {
    const live = lastWork([
      user,
      assistant("stream", [
        reasoning,
        text("先读取文件"),
        tool,
        reasoning,
        text("已经修复"),
      ]),
    ]);
    const history = lastWork([
      user,
      assistant("step", [reasoning, text("先读取文件"), tool]),
      assistant("reply", [reasoning, text("已经修复")]),
    ]);
    expect(live.answer?.parts).toEqual([text("已经修复")]);
    expect(history.answer?.parts).toEqual(live.answer?.parts);
    expect(history.history.flatMap((message) => message.parts)).toEqual(
      live.history.flatMap((message) => message.parts),
    );
    expect(live.defaultOpen).toBe(false);
    expect(history.defaultOpen).toBe(false);
  });

  it("keeps running, stopped, failed and incomplete work expanded", () => {
    for (const state of ["running", "interrupted", "failed"] as const) {
      const message = assistant("stream", [tool, text("部分回复")], {
        state,
      });
      message.isStreaming = state === "running";
      const work = lastWork([user, message]);
      expect(work.state).toBe(state);
      expect(work.defaultOpen).toBe(true);
      expect(work.answer).toBeUndefined();
      expect(work.history[0]?.parts).toContainEqual(text("部分回复"));
    }
    const incomplete = lastWork([
      user,
      assistant("step", [text("工具前的说明"), tool]),
    ]);
    expect(incomplete.answer).toBeUndefined();
    expect(incomplete.defaultOpen).toBe(true);
    expect(
      lastWork([user, assistant("reply", [text("部分回复")])], {
        interrupted: true,
      }).defaultOpen,
    ).toBe(true);
  });

  it("does not collapse an orphaned tool call behind a later partial reply", () => {
    const interruptedTool = { ...tool, status: "interrupted" } as DisplayPart;
    const work = lastWork([
      user,
      assistant("step", [interruptedTool]),
      assistant("partial", [text("部分输出")]),
    ]);
    expect(work.state).toBe("interrupted");
    expect(work.defaultOpen).toBe(true);
    expect(work.answer).toBeUndefined();
  });

  it("keeps rows after the authorized answer outside history and preserves tool cuts", () => {
    const step = assistant("step", [tool]);
    step.forkCut = { kind: "message", message_id: "result", side: "after" };
    const reply = assistant("reply", [reasoning, text("最终答复")]);
    reply.forkCut = { kind: "message", message_id: "reply", side: "after" };
    const tail = assistant("tail", [tool]);
    const work = lastWork([user, step, reply, tail]);
    expect(work.history[0]?.forkCut).toEqual(step.forkCut);
    expect(work.history[1]?.forkCut).toBeNull();
    expect(work.answer?.forkCut).toEqual(reply.forkCut);
    expect(work.following).toEqual([tail]);
  });

  it("keeps simple replies free of a redundant work history", () => {
    const work = lastWork([user, assistant("reply", [text("直接回答")])]);
    expect(work.history).toEqual([]);
    expect(work.answer?.parts).toEqual([text("直接回答")]);
  });

  it("keeps compaction and task notifications visible in their original order", () => {
    const compaction: DisplayPart = {
      kind: "compaction",
      summary: "摘要",
      triggeredBy: "auto",
      compactedMessages: 5,
      estTokensBefore: null,
      estTokensAfter: null,
    };
    const notification: DisplayMessage = {
      id: "notice",
      role: "notification",
      parts: [{ kind: "task_notification", tasks: [] }],
    };
    const units = buildChatRenderUnits([
      user,
      assistant("live", [tool, compaction, reasoning, text("完成")]),
      notification,
      assistant("wake", [text("后台任务结束")]),
    ]);
    expect(
      units.map((unit) =>
        unit.kind === "message" ? unit.message.role : unit.kind,
      ),
    ).toEqual(["user", "work", "compaction", "work", "notification", "work"]);
    expect((units[1] as ChatWork).history[0]?.parts).toEqual([tool]);
    expect((units[3] as ChatWork).history[0]?.parts).toEqual([reasoning]);
    expect((units[5] as ChatWork).answer?.parts).toEqual([
      text("后台任务结束"),
    ]);
  });

  it("does not keep the main turn running for a background task", () => {
    const background = {
      ...tool,
      details: { kind: "background_task", status: "running" },
    };
    const work = lastWork([
      { ...user, startedAt: 1000 },
      {
        ...assistant("reply", [background, text("已启动任务")], {
          state: "completed",
        }),
        finishedAt: 3000,
      },
    ]);
    expect(work.state).toBe("completed");
    expect(work.defaultOpen).toBe(false);
    expect(work.durationMs).toBe(2000);
  });

  it("keeps timing unknown when the message DTO has no timing fields", () => {
    const rows: ApiMessage[] = [
      {
        id: "u",
        role: "user",
        parts: [{ type: "text", text: "问题" }],
        created_at: "2026-10-21T00:00:00",
      },
      {
        id: "a",
        role: "assistant",
        parts: [
          { type: "reasoning", text: "过程" },
          { type: "text", text: "答案" },
        ],
        created_at: "2026-10-21T00:01:05Z",
      },
    ];
    const messages = storedToDisplayList(rows);
    expect(lastWork(messages).durationMs).toBeUndefined();
    expect(lastWork(messages).startedAt).toBeUndefined();
  });

  it("keeps a stable work key while streaming parts grow", () => {
    expect(lastWork([user, assistant("live", [tool])]).key).toBe(
      lastWork([user, assistant("live", [tool, reasoning, text("答案")])]).key,
    );
  });

  it("calculates completed duration from message timestamps across time zones", () => {
    const messages = storedToDisplayList([
      {
        id: "u",
        role: "user",
        parts: [{ type: "text", text: "问题" }],
        created_at: "2026-10-21T00:00:00",
        started_at: "2026-10-21T00:00:00Z",
      },
      {
        id: "a",
        role: "assistant",
        parts: [
          { type: "reasoning", text: "过程" },
          { type: "text", text: "完成" },
        ],
        created_at: "2026-10-21T01:02:03",
        finished_at: "2026-10-21T09:02:03+08:00",
      },
    ]);
    const result = lastWork(messages);
    expect(result.durationMs).toBe(3_723_000);
    expect(result.startedAt).toBe(Date.parse("2026-10-21T00:00:00Z"));
    expect(result.state).toBe("completed");
  });

  it("continues running work from the original user timestamp after reattachment", () => {
    const startedAt = Date.now() - 65_000;
    const live = assistant("live", [tool], { state: "running" });
    live.isStreaming = true;
    const result = lastWork([{ ...user, startedAt }, live]);
    expect(result.startedAt).toBe(startedAt);
    expect(result.state).toBe("running");
    expect(result.durationMs).toBe(0);
  });

  it("uses the last reply after resume while keeping the original turn start", () => {
    const result = lastWork([
      { ...user, startedAt: 1000 },
      { ...assistant("interrupted-step", [tool]), finishedAt: 66_000 },
      {
        ...assistant("resumed", [reasoning, text("完成")]),
        finishedAt: 666_000,
      },
    ]);
    expect(result.durationMs).toBe(665_000);
    expect(result.startedAt).toBe(1000);
    expect(result.state).toBe("completed");
  });

  it.each(["interrupted", "failed"] as const)(
    "does not calculate %s duration even when timestamps exist",
    (state) => {
      const result = lastWork([
        { ...user, startedAt: 1000 },
        {
          ...assistant("partial", [text("部分回复")], { state }),
          finishedAt: 666_000,
        },
      ]);
      expect(result.durationMs).toBeUndefined();
      expect(result.answer).toBeUndefined();
      expect(result.defaultOpen).toBe(true);
    },
  );

  it("starts notification work independently from its preceding user turn", () => {
    const result = lastWork([
      { ...user, startedAt: 1000 },
      { ...assistant("parent", [tool, text("已启动")]), finishedAt: 3000 },
      {
        id: "notification",
        role: "notification",
        parts: [],
        startedAt: 900_000,
      },
      {
        ...assistant("wake", [reasoning, text("后台结果")]),
        finishedAt: 901_000,
      },
    ]);
    expect(result.durationMs).toBe(1000);
    expect(result.startedAt).toBe(900_000);
  });

  it("keeps invalid timestamps unknown", () => {
    const messages = storedToDisplayList([
      {
        id: "u",
        role: "user",
        parts: [],
        created_at: "2026-10-21T00:00:00",
        started_at: "invalid",
      },
      {
        id: "a",
        role: "assistant",
        parts: [{ type: "text", text: "完成" }],
        created_at: "2026-10-21T00:01:00",
        finished_at: "invalid",
      },
    ]);
    expect(lastWork(messages).durationMs).toBeUndefined();
  });
});

it("shows hours and days only once their thresholds are reached", () => {
  expect(
    [
      0, 999, 1000, 59_999, 60_000, 119_999, 3_599_999, 3_600_000, 3_723_000,
      86_399_999, 86_400_000, 93_784_000, 172_800_000,
    ].map(formatWorkDuration),
  ).toEqual([
    "0分0秒",
    "0分0秒",
    "0分1秒",
    "0分59秒",
    "1分0秒",
    "1分59秒",
    "59分59秒",
    "1小时0分0秒",
    "1小时2分3秒",
    "23小时59分59秒",
    "1天0小时0分0秒",
    "1天2小时3分4秒",
    "2天0小时0分0秒",
  ]);
});
