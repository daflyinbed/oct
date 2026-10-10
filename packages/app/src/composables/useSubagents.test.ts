import { beforeEach, describe, expect, it, vi } from "vitest";
import client from "@/api/client";
import { useChat } from "@/composables/useChat";
import {
  applySubagentEvent,
  cancelSubagent,
  formatDurationMs,
  formatSubagentElapsed,
  openSubagentPanel,
  refreshSubagents,
  subagentStatusDot,
  subagentStatusLabel,
  taskByAgentId,
  taskByCallId,
  useSubagents,
} from "@/composables/useSubagents";
import { useTabs } from "@/composables/useTabs";
import type {
  SubagentTaskDto,
  SubagentUpdatedEventData,
} from "@/composables/useSubagents";

// store 走 openapi-fetch client，这里整体 mock 掉（不依赖 schema.d.ts）。
vi.mock("@/api/client", () => ({
  default: { GET: vi.fn(), POST: vi.fn() },
}));

const GET = vi.mocked(client.GET);
const POST = vi.mocked(client.POST);

const { tasks, loadedConversationId, runningCount, hasLost, subagentsFor } =
  useSubagents();
const { sendMessage } = useChat();
const { tabs } = useTabs();

const T0 = "2026-09-24T00:00:00";

function dto(
  runId: string,
  overrides: Partial<SubagentTaskDto> = {},
): SubagentTaskDto {
  return {
    run_id: runId,
    agent_id: `agent-${runId}`,
    description: `任务 ${runId}`,
    status: "running",
    execution_epoch: 1,
    parent_call_id: `call-${runId}`,
    provider_id: null,
    model_id: null,
    input_tokens: null,
    output_tokens: null,
    created_at: T0,
    finished_at: null,
    final_message_id: null,
    title: `任务 ${runId}`,
    running: true,
    ...overrides,
  };
}

function event(
  runId: string,
  overrides: Partial<SubagentUpdatedEventData> = {},
): SubagentUpdatedEventData {
  return {
    run_id: runId,
    agent_id: `agent-${runId}`,
    parent_call_id: `call-${runId}`,
    status: "running",
    execution_epoch: 1,
    description: `任务 ${runId}`,
    activity: null,
    ...overrides,
  };
}

/** 用 SSE body 生成 Response（与 useChat.test.ts 相同的构造方式）。 */
function sse(events: unknown[]): Response {
  const encoder = new TextEncoder();
  const stream = new ReadableStream<Uint8Array>({
    start(controller) {
      controller.enqueue(
        encoder.encode(
          `${events.map((e) => `data: ${JSON.stringify(e)}`).join("\n\n")}\n\n`,
        ),
      );
      controller.close();
    },
  });
  return new Response(stream, {
    status: 200,
    headers: { "content-type": "text/event-stream" },
  });
}

beforeEach(() => {
  GET.mockReset();
  POST.mockReset();
  vi.unstubAllGlobals();
  void refreshSubagents(null);
  // 标签状态是模块级单例（readonly，外部无法清空）：本文件只有
  // openSubagentPanel 用例写标签，用例内以「净增 + 去重」方式断言。
});

// ---------------------------------------------------------------------------
// refreshSubagents：快照拉取 + 迟到响应守卫
// ---------------------------------------------------------------------------

describe("refreshSubagents", () => {
  it("gET 子任务端点并写入快照（running 优先、created_at 升序）", async () => {
    GET.mockResolvedValue({
      data: [
        dto("r-done", { status: "completed", running: false, finished_at: T0 }),
        dto("r-run-b", {
          created_at: "2026-09-24T00:00:02",
        }),
        dto("r-run-a", { created_at: "2026-09-24T00:00:01" }),
      ],
      error: undefined,
    } as never);

    await refreshSubagents("conv-1");

    expect(GET).toHaveBeenCalledWith("/api/conversations/{id}/subagents", {
      params: { path: { id: "conv-1" } },
    });
    expect(loadedConversationId.value).toBe("conv-1");
    expect(subagentsFor("conv-1").value.map((t) => t.run_id)).toEqual([
      "r-run-a",
      "r-run-b",
      "r-done",
    ]);
    // 事件派生的 activity 快照拉取后清零
    expect(tasks.value.every((t) => t.activity === null)).toBe(true);
  });

  it("旧会话的迟到响应不得覆盖新会话（seq 守卫）", async () => {
    let resolveA: (value: unknown) => void = () => {};
    let resolveB: (value: unknown) => void = () => {};
    GET.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          resolveA = resolve;
        }) as never,
    );
    GET.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          resolveB = resolve;
        }) as never,
    );

    const first = refreshSubagents("conv-a");
    const second = refreshSubagents("conv-b");

    resolveA({ data: [dto("stale-a")], error: undefined });
    await first;
    expect(tasks.value.map((t) => t.run_id)).toEqual([]);

    resolveB({ data: [dto("fresh-b")], error: undefined });
    await second;
    expect(tasks.value.map((t) => t.run_id)).toEqual(["fresh-b"]);
    expect(loadedConversationId.value).toBe("conv-b");
  });

  it("conversationId 为 null 时清空任务与已加载会话", async () => {
    GET.mockResolvedValue({ data: [dto("r1")], error: undefined } as never);
    await refreshSubagents("conv-1");
    expect(tasks.value).toHaveLength(1);

    await refreshSubagents(null);
    expect(tasks.value).toHaveLength(0);
    expect(loadedConversationId.value).toBeNull();
    // 清空后派生列表也为空
    expect(subagentsFor("conv-1").value).toEqual([]);
  });

  it("请求失败保留旧列表", async () => {
    GET.mockResolvedValue({ data: [dto("keep")], error: undefined } as never);
    await refreshSubagents("conv-1");
    GET.mockResolvedValue({
      data: undefined,
      error: { error: "boom" },
    } as never);
    await refreshSubagents("conv-1");
    expect(tasks.value.map((t) => t.run_id)).toEqual(["keep"]);
  });
});

// ---------------------------------------------------------------------------
// applySubagentEvent：upsert / epoch 去重 / 跨会话隔离
// ---------------------------------------------------------------------------

describe("applySubagentEvent", () => {
  it("较新 epoch 的快照优先于 GET 期间到达的旧 epoch 终态事件", async () => {
    GET.mockResolvedValueOnce({ data: [dto("r1")] } as never);
    await refreshSubagents("conv-1");
    let finish!: (value: unknown) => void;
    GET.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          finish = resolve;
        }) as never,
    );
    const refreshing = refreshSubagents("conv-1");
    applySubagentEvent("conv-1", event("r1", { status: "completed" }));
    finish({ data: [dto("r1", { execution_epoch: 2 })] });
    await refreshing;
    expect(taskByAgentId("agent-r1")).toMatchObject({
      status: "running",
      execution_epoch: 2,
      finished_at: null,
    });
  });

  it("在途旧快照不能覆盖更新的终态、新任务或恢复 epoch", async () => {
    GET.mockResolvedValueOnce({ data: [dto("r1"), dto("r2")] } as never);
    await refreshSubagents("conv-1");
    let finish!: (value: unknown) => void;
    GET.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          finish = resolve;
        }) as never,
    );
    const refreshing = refreshSubagents("conv-1");
    applySubagentEvent("conv-1", event("r1", { status: "completed" }));
    applySubagentEvent(
      "conv-1",
      event("r2", { execution_epoch: 2, activity: "recovered" }),
    );
    applySubagentEvent("conv-1", event("r3"));
    finish({ data: [dto("r1"), dto("r2")] });
    await refreshing;
    expect(taskByAgentId("agent-r1")?.status).toBe("completed");
    expect(taskByAgentId("agent-r2")).toMatchObject({
      execution_epoch: 2,
      activity: "recovered",
    });
    expect(taskByAgentId("agent-r3")?.status).toBe("running");
  });

  it("同 epoch 的 running 重放不能回退终态，较新 epoch 清除结束时间", async () => {
    GET.mockResolvedValueOnce({
      data: [dto("r1", { status: "completed", finished_at: T0 })],
    } as never);
    await refreshSubagents("conv-1");
    applySubagentEvent("conv-1", event("r1"));
    expect(taskByAgentId("agent-r1")?.status).toBe("completed");
    applySubagentEvent("conv-1", event("r1", { execution_epoch: 2 }));
    expect(taskByAgentId("agent-r1")).toMatchObject({
      status: "running",
      execution_epoch: 2,
      finished_at: null,
    });
  });

  it("新事件插入任务行（status/running 派生，activity 保留）", async () => {
    GET.mockResolvedValue({ data: [], error: undefined } as never);
    await refreshSubagents("conv-1");

    applySubagentEvent(
      "conv-1",
      event("r-new", { activity: "正在执行 cat src/main.rs" }),
    );

    expect(tasks.value).toHaveLength(1);
    const task = tasks.value[0]!;
    expect(task.agent_id).toBe("agent-r-new");
    expect(task.status).toBe("running");
    expect(task.running).toBe(true);
    expect(task.activity).toBe("正在执行 cat src/main.rs");
    expect(task.created_at).toBeTypeOf("string");
  });

  it("同 run_id upsert：running → completed 转换并落 finished_at", async () => {
    GET.mockResolvedValue({ data: [dto("r1")], error: undefined } as never);
    await refreshSubagents("conv-1");

    applySubagentEvent(
      "conv-1",
      event("r1", { status: "completed", activity: null }),
    );

    const task = tasks.value[0]!;
    expect(task.status).toBe("completed");
    expect(task.running).toBe(false);
    expect(task.finished_at).toBeTypeOf("string");
    // 再次 running（新执行实例）：finished_at 清空由状态派生，epoch 递增
    applySubagentEvent(
      "conv-1",
      event("r1", { status: "running", execution_epoch: 2 }),
    );
    expect(tasks.value[0]!.status).toBe("running");
    expect(tasks.value[0]!.running).toBe(true);
    expect(tasks.value[0]!.execution_epoch).toBe(2);
  });

  it("迟到的小 epoch 事件（恢复前旧实例）被忽略", async () => {
    GET.mockResolvedValue({
      data: [dto("r1", { execution_epoch: 3 })],
      error: undefined,
    } as never);
    await refreshSubagents("conv-1");

    applySubagentEvent(
      "conv-1",
      event("r1", { status: "completed", execution_epoch: 1 }),
    );
    expect(tasks.value[0]!.status).toBe("running");
    expect(tasks.value[0]!.execution_epoch).toBe(3);

    // 同 epoch 的事件允许（幂等重放）
    applySubagentEvent(
      "conv-1",
      event("r1", { status: "failed", execution_epoch: 3 }),
    );
    expect(tasks.value[0]!.status).toBe("failed");
  });

  it("别的会话的事件不得插入当前列表（跨会话隔离）", async () => {
    GET.mockResolvedValue({ data: [], error: undefined } as never);
    await refreshSubagents("conv-1");

    applySubagentEvent("conv-other", event("r-foreign"));
    expect(tasks.value).toHaveLength(0);

    // 已在列表中的任务仍可被更新（它属于已加载会话）
    applySubagentEvent("conv-1", event("r-local"));
    applySubagentEvent("conv-1", event("r-local", { status: "completed" }));
    expect(tasks.value[0]!.status).toBe("completed");
  });
});

// ---------------------------------------------------------------------------
// 派生状态：runningCount / hasLost / taskByCallId / taskByAgentId
// ---------------------------------------------------------------------------

describe("派生状态与查找", () => {
  it("runningCount / hasLost 按当前任务集合计算", async () => {
    GET.mockResolvedValue({
      data: [
        dto("r1", { status: "running" }),
        dto("r2", { status: "running" }),
        dto("r3", { status: "lost" }),
      ],
      error: undefined,
    } as never);
    await refreshSubagents("conv-1");

    expect(runningCount.value).toBe(2);
    expect(hasLost.value).toBe(true);

    applySubagentEvent("conv-1", event("r1", { status: "completed" }));
    expect(runningCount.value).toBe(1);

    await refreshSubagents(null);
    expect(runningCount.value).toBe(0);
    expect(hasLost.value).toBe(false);
  });

  it("taskByCallId / taskByAgentId 命中查找", async () => {
    GET.mockResolvedValue({ data: [dto("r1")], error: undefined } as never);
    await refreshSubagents("conv-1");

    expect(taskByCallId("call-r1")?.agent_id).toBe("agent-r1");
    expect(taskByCallId("missing")).toBeNull();
    expect(taskByAgentId("agent-r1")?.run_id).toBe("r1");
    expect(taskByAgentId("missing")).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// openSubagentPanel / cancelSubagent
// ---------------------------------------------------------------------------

describe("openSubagentPanel / cancelSubagent", () => {
  it("打开 subagent 标签（携带根会话 id），重复打开去重激活", () => {
    openSubagentPanel("agent-1", "定位 Bug", "root-conv");

    expect(tabs.value).toHaveLength(1);
    expect(tabs.value[0]).toMatchObject({
      id: "subagent:agent-1",
      kind: "subagent",
      payload: "agent-1",
      title: "子代理 · 定位 Bug",
      rootConversationId: "root-conv",
    });

    openSubagentPanel("agent-1", "定位 Bug", "root-conv");
    expect(tabs.value).toHaveLength(1);

    // 相同标题的不同 agent 各有标签
    openSubagentPanel("agent-2", "定位 Bug");
    expect(tabs.value).toHaveLength(2);
    expect(tabs.value[1]!.rootConversationId).toBeNull();
  });

  it("cancelSubagent POST 到取消端点", async () => {
    POST.mockResolvedValue({ data: {}, error: undefined } as never);
    await cancelSubagent("agent-1");
    expect(POST).toHaveBeenCalledWith("/api/subagents/{agentId}/cancel", {
      params: { path: { agentId: "agent-1" } },
    });
  });
});

// ---------------------------------------------------------------------------
// 单例 reducer 派发：subagent_updated → 任务 store（显式接线验证）
// ---------------------------------------------------------------------------

describe("useChat 单例的 subagent_updated 派发", () => {
  it("主面板流中的 subagent_updated 进入任务 store（带流所属会话 id）", async () => {
    GET.mockResolvedValue({ data: [], error: undefined } as never);
    await refreshSubagents("conv-root");

    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(
        sse([
          {
            type: "subagent_updated",
            data: event("r-live", { activity: "正在读取 useChat.ts" }),
          },
          { type: "text_delta", data: "等待子任务" },
          { type: "finish" },
        ]),
      ),
    );

    await sendMessage("conv-root", "hi");

    expect(tasks.value.map((t) => t.run_id)).toEqual(["r-live"]);
    expect(tasks.value[0]!.activity).toBe("正在读取 useChat.ts");
    // 事件不进消息内容
    const { messages } = useChat();
    expect(
      messages.value.some((m) =>
        m.parts.some((p) => p.kind === "text" && p.text.includes("r-live")),
      ),
    ).toBe(false);
  });

  it("别的会话的流（视图切走后收尾）不污染当前已加载列表", async () => {
    GET.mockResolvedValue({ data: [], error: undefined } as never);
    await refreshSubagents("conv-current");

    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(
        sse([
          {
            type: "subagent_updated",
            data: event("r-foreign", { parent_call_id: "call-x" }),
          },
          { type: "finish" },
        ]),
      ),
    );
    await sendMessage("conv-stale", "hi");

    expect(tasks.value).toHaveLength(0);
  });
});

// ---------------------------------------------------------------------------
// 展示助手
// ---------------------------------------------------------------------------

describe("状态与耗时格式化", () => {
  it("状态点/文案按状态映射", () => {
    expect(subagentStatusDot("running")).toBe("bg-accent-7");
    expect(subagentStatusDot("lost")).toBe("bg-warning-10");
    expect(subagentStatusLabel("running")).toBe("运行中");
    expect(subagentStatusLabel("lost")).toBe("待恢复");
    expect(subagentStatusLabel("completed")).toBe("已完成");
  });

  it("耗时格式：终态取 finished_at - created_at，运行中以 now 计", () => {
    const now = Date.parse("2026-09-24T00:01:00");
    expect(formatSubagentElapsed("2026-09-24T00:00:30", null, now)).toBe("30s");
    expect(
      formatSubagentElapsed("2026-09-24T00:00:00", "2026-09-24T00:00:24", now),
    ).toBe("24s");
    expect(
      formatSubagentElapsed("2026-09-24T00:00:00", "2026-09-24T00:01:23", now),
    ).toBe("1m 23s");
    expect(formatSubagentElapsed(null, null, now)).toBe("—");
    // 倒挂/无法解析
    expect(
      formatSubagentElapsed("2026-09-24T00:02:00", "2026-09-24T00:00:00", now),
    ).toBe("—");
    expect(formatDurationMs(3700 * 1000)).toBe("1h 1m");
  });
});
