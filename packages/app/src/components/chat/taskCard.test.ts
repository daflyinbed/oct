import { beforeEach, describe, expect, it, vi } from "vitest";
import client from "@/api/client";
import {
  parseTaskDescription,
  resolveTaskCard,
  taskResultSummary,
} from "@/components/chat/taskCard";
import { refreshSubagents, taskByCallId } from "@/composables/useSubagents";
import type { SubagentTaskDto } from "@/composables/useSubagents";

vi.mock("@/api/client", () => ({
  default: { GET: vi.fn(), POST: vi.fn() },
}));

const GET = vi.mocked(client.GET);

const T0 = "2026-09-24T00:00:00";

function dto(
  runId: string,
  overrides: Partial<SubagentTaskDto> = {},
): SubagentTaskDto {
  return {
    run_id: runId,
    agent_id: `agent-${runId}`,
    description: "定位 SSE 重连 Bug",
    status: "running",
    execution_epoch: 1,
    parent_call_id: "call-1",
    provider_id: null,
    model_id: null,
    input_tokens: null,
    output_tokens: null,
    created_at: T0,
    finished_at: null,
    final_message_id: null,
    title: "定位 SSE 重连 Bug",
    running: true,
    ...overrides,
  };
}

/** task 工具调用的 ToolCallPart 最小形状（resolveTaskCard 只读这些字段）。 */
function taskPart(
  overrides: Partial<{
    title: string | null;
    arguments: string;
    status: string;
    details: Record<string, unknown> | null;
  }> = {},
) {
  return {
    title: null,
    arguments: "",
    status: "running",
    details: null,
    ...overrides,
  };
}

beforeEach(() => {
  GET.mockReset();
  void refreshSubagents(null);
});

describe("parseTaskDescription", () => {
  it("解析 arguments JSON 的 description", () => {
    expect(
      parseTaskDescription(
        String.raw`{"description":"定位 SSE 重连 Bug","prompt":"检查 useChat…"}`,
      ),
    ).toBe("定位 SSE 重连 Bug");
  });

  it("半截 JSON / 缺字段 / 非 JSON 返回 null（回退标题）", () => {
    expect(parseTaskDescription('{"descr')).toBeNull();
    expect(parseTaskDescription('{"prompt":"x"}')).toBeNull();
    expect(parseTaskDescription("")).toBeNull();
  });
});

describe("taskResultSummary", () => {
  it("信封 JSON 取 output 头部（约 200 字符）", () => {
    const output = "x".repeat(260);
    const summary = taskResultSummary(
      JSON.stringify({
        agent_id: "a",
        run_id: "r",
        status: "completed",
        output,
      }),
    );
    expect(summary).toBe(`${"x".repeat(200)}…`);
  });

  it("非信封内容取原文头部；null/空白返回 null", () => {
    expect(taskResultSummary("原因：A 文件第 3 行")).toBe(
      "原因：A 文件第 3 行",
    );
    expect(taskResultSummary(null)).toBeNull();
    expect(taskResultSummary("   \n")).toBeNull();
  });
});

describe("resolveTaskCard（store 优先级）", () => {
  it("运行中：store 命中（按父 call id）提供 agent_id/状态/活动", async () => {
    GET.mockResolvedValue({ data: [dto("r1")], error: undefined } as never);
    await refreshSubagents("conv-1");

    const state = resolveTaskCard(
      taskPart({
        arguments: String.raw`{"description":"定位 SSE 重连 Bug","prompt":"…"}`,
      }),
      taskByCallId("call-1"),
    );

    expect(state.description).toBe("定位 SSE 重连 Bug");
    expect(state.status).toBe("running");
    expect(state.agentId).toBe("agent-r1");
    expect(state.createdAt).toBe(T0);
  });

  it("「打开子代理」在结果落地前即可用（store 命中即有 agent_id）", async () => {
    GET.mockResolvedValue({ data: [dto("r1")], error: undefined } as never);
    await refreshSubagents("conv-1");

    // 卡片仍是 running（tool_result 未到）：details 为 null
    const state = resolveTaskCard(taskPart(), null);
    expect(state.status).toBe("running");
    expect(state.agentId).toBeNull(); // store 未命中（不传 task）时运行中无 agent_id

    const withStore = resolveTaskCard(taskPart(), taskByCallId("call-1"));
    expect(withStore.agentId).toBe("agent-r1");
  });

  it("终态回放：details（kind=subagent_task）提供 agent_id 与状态", () => {
    const state = resolveTaskCard(
      taskPart({
        status: "done",
        details: {
          kind: "subagent_task",
          agent_id: "agent-done",
          run_id: "r-done",
          description: "定位 SSE 重连 Bug",
          status: "completed",
        },
      }),
      null,
    );
    expect(state.status).toBe("completed");
    expect(state.agentId).toBe("agent-done");
    expect(state.description).toBe("定位 SSE 重连 Bug");
  });

  it("兜底链：interrupted → lost（待恢复）；generating → running；标题兜底描述", () => {
    const interrupted = resolveTaskCard(
      taskPart({ status: "interrupted", title: "定位 SSE 重连 Bug" }),
      null,
    );
    expect(interrupted.status).toBe("lost");
    expect(interrupted.description).toBe("定位 SSE 重连 Bug");

    const generating = resolveTaskCard(
      taskPart({ status: "generating", arguments: '{"desc' }),
      null,
    );
    expect(generating.status).toBe("running");
    expect(generating.description).toBe("子代理任务"); // 无标题无参数的兜底
  });
});
