import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import client from "@/api/client";
import {
  createChatSession,
  onTitleUpdated,
  sendFailureText,
  useChat,
} from "@/composables/useChat";
import {
  clearContextUsage,
  useContextUsage,
} from "@/composables/useContextUsage";
import type { components } from "@/api/schema";
import type {
  CompactionPart,
  ReasoningPart,
  ToolCallPart,
} from "@/composables/useChat";
import type { DeepReadonly } from "vue";

// fetchMessages 走 openapi-fetch client，这里整体 mock 掉。
vi.mock("@/api/client", () => ({
  default: { GET: vi.fn(), POST: vi.fn() },
}));

const GET = vi.mocked(client.GET);
const POST = vi.mocked(client.POST);

const {
  messages,
  sending,
  resumable,
  pendingApprovals,
  fetchMessages,
  getLastUserRequest,
  sendMessage,
  resumeTurn,
  attachRun,
  decideApproval,
  clearMessages,
} = useChat();

// ---------------------------------------------------------------------------
// 测试数据构造
// ---------------------------------------------------------------------------

let membershipRevision = 0;
const membershipRevisions = new Map<string, number>();

/** 后端 MessageDto 的最小形状（仅用到本 composable 读的字段）。 */
function storedMessage(
  role: string,
  parts: string | components["schemas"]["MessagePart"][],
  extra: Partial<components["schemas"]["MessageDto"]> = {},
): components["schemas"]["MessageDto"] {
  const message = {
    id: `msg-${Math.random().toString(36).slice(2)}`,
    role,
    parts: typeof parts === "string" ? legacyParts(parts) : parts,
    details: null,
    ordering: 0,
    appended_at_revision: ++membershipRevision,
    created_at: "2026-09-20T00:00:00",
    ...extra,
  };
  membershipRevisions.set(message.id, message.appended_at_revision ?? 0);
  return message;
}

/**
 * 测试简写：externally-tagged serde JSON 字符串（旧 parts_json 线格式）
 * → 类型化 parts。损坏负载按后端 DTO 规则降级为单一 text part。
 */
function legacyParts(
  partsJson: string,
): components["schemas"]["MessagePart"][] {
  type Raw = Record<string, any>;
  try {
    const parsed = JSON.parse(partsJson);
    if (Array.isArray(parsed)) {
      return (parsed as Raw[]).map((raw) => {
        if (typeof raw.Text === "string") {
          return { type: "text", text: raw.Text };
        }
        if (typeof raw.Reasoning === "string") {
          return { type: "reasoning", text: raw.Reasoning };
        }
        if (raw.ToolCall && typeof raw.ToolCall === "object") {
          return { type: "tool_call", ...raw.ToolCall };
        }
        if (raw.ToolResult && typeof raw.ToolResult === "object") {
          return { type: "tool_result", ...raw.ToolResult };
        }
        throw new Error("unknown part shape");
      });
    }
  } catch {
    // fall through to the degraded single-text form
  }
  return [{ type: "text", text: partsJson }];
}

/** 用 SSE body 生成 Response（可按任意字符串切块，模拟网络分片）。 */
function sseResponse(chunks: string[]): Response {
  const encoder = new TextEncoder();
  const stream = new ReadableStream<Uint8Array>({
    start(controller) {
      for (const chunk of chunks) controller.enqueue(encoder.encode(chunk));
      controller.close();
    },
  });
  return new Response(stream, {
    status: 200,
    headers: { "content-type": "text/event-stream" },
  });
}

function sse(events: unknown[]): Response {
  return sseResponse([
    `${events.map((e) => `data: ${JSON.stringify(e)}`).join("\n\n")}\n\n`,
  ]);
}

// AgentEvent 线格式（serde tag="type" content="data"）
const textDelta = (data: string) => ({ type: "text_delta", data });
const reasoningDelta = (data: string) => ({ type: "reasoning_delta", data });
function toolCallDelta(
  call_id: string,
  arguments_delta: string,
  name: string | null = null,
) {
  return { type: "tool_call_delta", data: { call_id, name, arguments_delta } };
}
function toolCallStart(
  id: string,
  name: string,
  title: string,
  arguments_: string,
) {
  return {
    type: "tool_call_start",
    data: { id, name, title, arguments: arguments_ },
  };
}
function toolOutputDelta(
  call_id: string,
  stream: "stdout" | "stderr",
  delta: string,
) {
  return { type: "tool_output_delta", data: { call_id, stream, delta } };
}
function toolResult(
  call_id: string,
  content: string,
  is_error = false,
  details: Record<string, unknown> | null = null,
) {
  return {
    type: "tool_result",
    data: { call_id, content, is_error, details },
  };
}
const finishEvent = { type: "finish" };
const cancelledEvent = { type: "cancelled" };
const errorEvent = (data: string) => ({ type: "error", data });
// 执行日志的 ready 边界控制事件，不进入消息 reducer。
function runMeta(start_message_id: string) {
  return {
    type: "run_meta",
    data: {
      run_id: "test-run",
      execution_epoch: 1,
      history_epoch: 0,
      replay_after_revision: membershipRevisions.get(start_message_id) ?? 0,
      trigger_message_id: start_message_id,
    },
  };
}
// 重放重建的流式消息 id 前缀（attachRun 内约定）
const RE_reattachedId = /^reattached-/;

function toolCards(): DeepReadonly<ToolCallPart>[] {
  return messages.value.flatMap((m) =>
    m.parts.filter(
      (p): p is DeepReadonly<ToolCallPart> => p.kind === "tool_call",
    ),
  );
}

function reasoningParts(): DeepReadonly<ReasoningPart>[] {
  return messages.value.flatMap((m) =>
    m.parts.filter(
      (p): p is DeepReadonly<ReasoningPart> => p.kind === "reasoning",
    ),
  );
}

let convSeq = 0;
/** 每个用例用独立会话 id，避开 fetchMessages 的 200ms 去重守卫。 */
function nextConvId(): string {
  convSeq += 1;
  return `conv-${convSeq}`;
}

beforeEach(() => {
  clearMessages();
  clearContextUsage();
  membershipRevision = 0;
  membershipRevisions.clear();
  GET.mockReset();
  POST.mockReset();
  vi.unstubAllGlobals();
});

afterEach(() => {
  vi.unstubAllGlobals();
});

// ---------------------------------------------------------------------------
// 历史回放（fetchMessages → storedToDisplayList）
// ---------------------------------------------------------------------------

describe("fetchMessages 历史回放", () => {
  it("用户与助手的文本消息各自渲染为气泡", async () => {
    GET.mockResolvedValue({
      data: [
        storedMessage("user", '[{"Text":"你好"}]'),
        storedMessage("assistant", '[{"Text":"先分析"},{"Text":"再回答"}]'),
      ],
    } as never);

    await fetchMessages(nextConvId());

    expect(messages.value.map((m) => m.role)).toEqual(["user", "assistant"]);
    expect(messages.value[1]?.parts).toEqual([
      { kind: "text", text: "先分析再回答" },
    ]);
  });

  it("tool 消息回填到匹配的工具卡片（content/details/title）", async () => {
    GET.mockResolvedValue({
      data: [
        storedMessage("user", '[{"Text":"读一下"}]'),
        storedMessage(
          "assistant",
          String.raw`[{"ToolCall":{"id":"c1","name":"read_file","arguments":"{\"path\":\"a.txt\"}"}}]`,
        ),
        storedMessage(
          "tool",
          '[{"ToolResult":{"call_id":"c1","content":"1. hi","is_error":false}}]',
          { details: { extra: { title: "a.txt", total_lines: 1 } } },
        ),
      ],
    } as never);

    await fetchMessages(nextConvId());

    // tool 消息不单独成气泡，而是填充进 assistant 的卡片
    expect(messages.value.map((m) => m.role)).toEqual(["user", "assistant"]);
    const card = toolCards()[0]!;
    expect(card.name).toBe("read_file");
    expect(card.status).toBe("done");
    expect(card.content).toBe("1. hi");
    expect(card.isError).toBe(false);
    expect(card.title).toBe("a.txt");
    expect(card.details).toEqual({ title: "a.txt", total_lines: 1 });
  });

  it("历史里的 Reasoning part 渲染为思考卡片，时长来自 details", async () => {
    GET.mockResolvedValue({
      data: [
        storedMessage("user", '[{"Text":"问"}]'),
        storedMessage("assistant", '[{"Reasoning":"历史思考"},{"Text":"答"}]', {
          details: { reasoning_duration_ms: 4200 },
        }),
        // 无 details 的旧数据：卡片仍在，只是不显示时长
        storedMessage("assistant", '[{"Reasoning":"旧思考"}]'),
      ],
    } as never);

    await fetchMessages(nextConvId());

    expect(messages.value[1]?.parts).toEqual([
      {
        kind: "reasoning",
        text: "历史思考",
        startedAt: null,
        durationMs: 4200,
        ended: true,
      },
      { kind: "text", text: "答" },
    ]);
    expect(messages.value[2]?.parts).toEqual([
      {
        kind: "reasoning",
        text: "旧思考",
        startedAt: null,
        durationMs: null,
        ended: true,
      },
    ]);
  });

  it("孤儿 tool result（无匹配卡片）落到最近的 assistant 消息上", async () => {
    GET.mockResolvedValue({
      data: [
        storedMessage("user", '[{"Text":"旧数据"}]'),
        storedMessage(
          "tool",
          '[{"ToolResult":{"call_id":"orphan","content":"x","is_error":true}}]',
        ),
      ],
    } as never);

    await fetchMessages(nextConvId());

    const cards = toolCards();
    expect(cards).toHaveLength(1);
    expect(cards[0]!.name).toBe("unknown");
    expect(cards[0]!.isError).toBe(true);
    // 宿主是补建的 assistant 消息
    expect(messages.value.at(-1)?.role).toBe("assistant");
  });

  it("无法解析的 parts_json 降级为单一文本 part", async () => {
    GET.mockResolvedValue({
      data: [storedMessage("user", "not-json-at-all")],
    } as never);

    await fetchMessages(nextConvId());

    expect(messages.value[0]?.parts).toEqual([
      { kind: "text", text: "not-json-at-all" },
    ]);
  });

  it("悬空 ToolCall（无配对 ToolResult）渲染为 interrupted 状态", async () => {
    GET.mockResolvedValue({
      data: [
        storedMessage("user", '[{"Text":"读一下"}]'),
        storedMessage(
          "assistant",
          String.raw`[{"ToolCall":{"id":"c1","name":"read_file","arguments":"{\"path\":\"a.txt\"}"}}]`,
        ),
        // 同批已完成配对的调用不受影响
        storedMessage(
          "assistant",
          '[{"ToolCall":{"id":"c2","name":"list_dir","arguments":"{}"}}]',
        ),
        storedMessage(
          "tool",
          '[{"ToolResult":{"call_id":"c2","content":"ok","is_error":false}}]',
        ),
      ],
    } as never);

    await fetchMessages(nextConvId());

    const cards = toolCards().sort((a, b) => a.callId.localeCompare(b.callId));
    expect(cards).toHaveLength(2);
    expect(cards[0]!.status).toBe("interrupted"); // c1：始终没等到结果
    expect(cards[0]!.content).toBeNull();
    expect(cards[1]!.status).toBe("done"); // c2：已配对
    expect(cards[1]!.content).toBe("ok");
  });

  it("请求失败时保持现有消息不变", async () => {
    GET.mockResolvedValue({ data: undefined, error: {} } as never);
    await fetchMessages(nextConvId());
    expect(messages.value).toHaveLength(0);
  });
});

// ---------------------------------------------------------------------------
// 上一笔请求水合（getLastUserRequest → useProviders.hydrateLastRequest）
// ---------------------------------------------------------------------------

describe("getLastUserRequest 上一笔请求", () => {
  it("从尾部找最后一条带 provider/model 的 user 行（details.extra.reasoning 同源）", async () => {
    GET.mockResolvedValue({
      data: [
        storedMessage("user", '[{"Text":"第一笔"}]', {
          provider_id: "p1",
          model_id: "m1",
          details: { extra: { reasoning: { type: "effort", effort: "high" } } },
        }),
        storedMessage("assistant", '[{"Text":"答"}]', {
          provider_id: "p1",
          model_id: "m1",
        }),
        storedMessage("user", '[{"Text":"第二笔用默认档"}]', {
          provider_id: "p2",
          model_id: "m2",
        }),
      ],
    } as never);

    const conversationId = nextConvId();
    await fetchMessages(conversationId);

    expect(getLastUserRequest(conversationId)).toEqual({
      providerId: "p2",
      modelId: "m2",
      reasoning: null,
    });
  });

  it("无带 provider/model 的 user 行时返回 null", async () => {
    GET.mockResolvedValue({
      data: [storedMessage("assistant", '[{"Text":"hi"}]')],
    } as never);
    const conversationId = nextConvId();
    await fetchMessages(conversationId);
    expect(getLastUserRequest(conversationId)).toBeNull();
  });

  it("历史只属于当前会话，切换期间及加载失败时不沿用旧请求", async () => {
    const session = createChatSession({ contextUsage: false });
    GET.mockResolvedValueOnce({
      data: [
        storedMessage("user", "first", { provider_id: "p1", model_id: "m1" }),
      ],
    } as never);
    await session.fetchMessages("first");
    expect(session.getLastUserRequest("first")?.modelId).toBe("m1");
    expect(session.getLastUserRequest("second")).toBeNull();

    let fail!: (value: unknown) => void;
    GET.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          fail = resolve;
        }) as never,
    );
    const pending = session.fetchMessages("second");
    expect(session.getLastUserRequest("first")).toBeNull();
    expect(session.getLastUserRequest("second")).toBeNull();
    fail({ error: { message: "history unavailable" } });
    await pending;
    expect(session.getLastUserRequest("second")).toBeNull();
    session.dispose();
  });

  it("当前会话重新加载失败时不提供上一次历史快照", async () => {
    const session = createChatSession({ contextUsage: false });
    GET.mockResolvedValueOnce({
      data: [
        storedMessage("user", "first", { provider_id: "p1", model_id: "m1" }),
      ],
    } as never);
    await session.fetchMessages("first");
    GET.mockResolvedValueOnce({
      error: { message: "history unavailable" },
    } as never);
    await session.refreshMessages("first");
    expect(session.getLastUserRequest("first")).toBeNull();
    session.dispose();
  });

  it("清空会话后丢弃上一笔请求", async () => {
    const session = createChatSession({ contextUsage: false });
    GET.mockResolvedValueOnce({
      data: [
        storedMessage("user", "first", { provider_id: "p1", model_id: "m1" }),
      ],
    } as never);
    await session.fetchMessages("first");
    session.clearMessages();
    expect(session.getLastUserRequest("first")).toBeNull();
    session.dispose();
  });
});

// ---------------------------------------------------------------------------
// 实时流（sendMessage → fetch SSE → applyStreamEvent）
// ---------------------------------------------------------------------------

describe("sendMessage 实时流", () => {
  it("嵌套 MCP 事件只更新外层代码卡片，重放不新增卡片", async () => {
    const call = {
      id: "code1/1",
      name: "mcp__api__search",
      title: "api / search",
      status: "running",
      error: null,
    };
    const nested = (status: string) => ({
      type: "nested_tool_update",
      data: { parent_call_id: "code1", call: { ...call, status } },
    });
    stubFetch(
      sse([
        toolCallStart(
          "code1",
          "execute_code",
          "MCP code mode",
          '{"code":"text(42)"}',
        ),
        nested("running"),
        nested("completed"),
        nested("completed"),
        cancelledEvent,
      ]),
    );
    await sendMessage(nextConvId(), "code");
    expect(toolCards()).toHaveLength(1);
    expect(toolCards()[0]!.details?.calls).toEqual([
      { ...call, status: "completed" },
    ]);
  });
  function stubFetch(...responses: Response[]) {
    const fetchMock = vi.fn();
    for (const r of responses) fetchMock.mockResolvedValueOnce(r);
    vi.stubGlobal("fetch", fetchMock);
    return fetchMock;
  }

  it.each([
    [finishEvent, "completed"],
    [cancelledEvent, "interrupted"],
    [errorEvent("连接中断"), "failed"],
  ])("保留主轮终态，仅正常完成记录结束时间：%j", async (event, state) => {
    stubFetch(sse([textDelta("部分文字"), event]));
    const before = Date.now();
    await sendMessage(nextConvId(), "hi");
    const message = messages.value.at(-1)!;
    expect(message.work?.state).toBe(state);
    expect(message.startedAt).toBeGreaterThanOrEqual(before);
    if (state === "completed") {
      expect(message.finishedAt).toBeGreaterThanOrEqual(message.startedAt!);
      expect(message.finishedAt).toBeLessThanOrEqual(Date.now());
    } else expect(message.finishedAt).toBeUndefined();
  });

  it("发送前补投后台通知后，终态刷新仍保留当前轮的部分回复与取消状态", async () => {
    const conv = nextConvId();
    const previous = [
      storedMessage("user", [{ type: "text", text: "启动后台任务" }]),
      storedMessage("assistant", [{ type: "text", text: "任务已启动" }]),
    ];
    GET.mockResolvedValueOnce({ data: previous } as never);
    await fetchMessages(conv);

    let clientRequestId: string;
    vi.stubGlobal(
      "fetch",
      vi.fn<typeof fetch>(async (_path, init) => {
        clientRequestId = JSON.parse(init!.body as string).client_request_id;
        return sse([
          toolCallStart("call", "read_file", "a.txt", '{"path":"a.txt"}'),
          toolResult("call", "ok"),
          reasoningDelta("正在整理结果"),
          textDelta("尚未完成的回复"),
          cancelledEvent,
        ]);
      }),
    );
    GET.mockImplementation(async (path) => {
      if (path === "/api/conversations/{id}/run")
        return { data: { running: false, resumable: true } } as never;
      return {
        data: [
          ...previous,
          storedMessage("user", [{ type: "text", text: "后台任务已完成" }], {
            details: {
              extra: { kind: "background_task_notification", tasks: [] },
            },
          }),
          storedMessage("user", [{ type: "text", text: "检查" }], {
            id: "current-user",
            details: { extra: { client_request_id: clientRequestId } },
          }),
          storedMessage("assistant", [
            {
              type: "tool_call",
              id: "call",
              name: "read_file",
              arguments: "{}",
            },
          ]),
          storedMessage("tool", [
            {
              type: "tool_result",
              call_id: "call",
              content: "ok",
              is_error: false,
            },
          ]),
        ],
        response: new Response(null, {
          headers: { "X-Context-Revision": "7" },
        }),
      } as never;
    });

    await sendMessage(conv, "检查");

    expect(
      messages.value.filter((message) => message.role === "notification"),
    ).toHaveLength(1);
    expect(
      messages.value.findLast((message) => message.role === "user")?.id,
    ).toBe("current-user");
    expect(messages.value.at(-1)?.work?.state).toBe("interrupted");
    expect(messages.value.at(-1)?.parts).toEqual([
      expect.objectContaining({ kind: "reasoning", text: "正在整理结果" }),
      { kind: "text", text: "尚未完成的回复" },
    ]);
    expect(toolCards()).toHaveLength(1);
  });

  it.each([
    ["user", finishEvent],
    ["notification", finishEvent],
    ["user", cancelledEvent],
    ["notification", cancelledEvent],
  ])(
    "终态刷新包含下一轮 %s 时，不把旧轮 %j 状态或部分回复贴到新回复",
    async (nextRole, event) => {
      let clientRequestId: string;
      vi.stubGlobal(
        "fetch",
        vi.fn<typeof fetch>(async (_path, init) => {
          clientRequestId = JSON.parse(init!.body as string).client_request_id;
          return sse([textDelta("旧轮部分回复"), event]);
        }),
      );
      GET.mockImplementation(async (path) => {
        if (path === "/api/conversations/{id}/run")
          return { data: { running: false, resumable: false } } as never;
        return {
          data: [
            storedMessage("user", [{ type: "text", text: "hi" }], {
              details: { extra: { client_request_id: clientRequestId } },
            }),
            storedMessage("user", [{ type: "text", text: "hi" }], {
              details: {
                extra:
                  nextRole === "notification"
                    ? { kind: "background_task_notification", tasks: [] }
                    : { client_request_id: "next-request" },
              },
            }),
            storedMessage("assistant", [{ type: "text", text: "新轮回复" }]),
          ],
          response: new Response(null, {
            headers: { "X-Context-Revision": "4" },
          }),
        } as never;
      });
      await sendMessage(nextConvId(), "hi");
      expect(messages.value.at(-1)?.parts).toEqual([
        { kind: "text", text: "新轮回复" },
      ]);
      expect(messages.value.at(-1)?.work).toBeUndefined();
      expect(
        messages.value.flatMap((message) => message.parts),
      ).not.toContainEqual({ kind: "text", text: "旧轮部分回复" });
    },
  );

  it("文本 delta 累积为一个文本 part，finish 后结束流式态", async () => {
    stubFetch(
      sse([
        textDelta("你好"),
        textDelta("，世界"),
        reasoningDelta("思考中"), // 文本之后到来的思考独立成卡片，不打断文本
        {
          type: "usage",
          data: { input_tokens: 1, output_tokens: 2, reasoning_tokens: null },
        },
        finishEvent,
      ]),
    );

    await sendMessage(nextConvId(), "hi");

    // pending 用户消息 + 最终 assistant 消息
    expect(messages.value).toHaveLength(2);
    expect(messages.value[0]?.role).toBe("user");
    const assistant = messages.value[1]!;
    expect(assistant.role).toBe("assistant");
    expect(assistant.parts[0]).toEqual({ kind: "text", text: "你好，世界" });
    expect(assistant.parts[1]).toMatchObject({
      kind: "reasoning",
      text: "思考中",
      ended: true,
    });
    expect(assistant.isStreaming).toBe(false);
    expect(sending.value).toBe(false);
  });

  it("思考 delta 累积为一个 part，文本到来时结束并记时长", async () => {
    stubFetch(
      sse([
        reasoningDelta("想一想"),
        reasoningDelta("，再想想"),
        textDelta("答"),
        finishEvent,
      ]),
    );

    await sendMessage(nextConvId(), "hi");

    const assistant = messages.value[1]!;
    expect(assistant.parts).toHaveLength(2);
    const blocks = reasoningParts();
    expect(blocks).toHaveLength(1);
    expect(blocks[0]!.text).toBe("想一想，再想想");
    expect(blocks[0]!.ended).toBe(true);
    expect(blocks[0]!.durationMs).toBeTypeOf("number");
    expect(assistant.parts[1]).toEqual({ kind: "text", text: "答" });
  });

  it("多轮工具循环各有一个思考块，互不合并", async () => {
    stubFetch(
      sse([
        reasoningDelta("第一轮"),
        toolCallDelta("c1", "{}", "list_dir"),
        toolCallStart("c1", "list_dir", ".", "{}"),
        toolResult("c1", "ok"),
        reasoningDelta("第二轮"),
        textDelta("答"),
        finishEvent,
      ]),
    );

    await sendMessage(nextConvId(), "hi");

    const blocks = reasoningParts();
    expect(blocks.map((b) => b.text)).toEqual(["第一轮", "第二轮"]);
    expect(blocks.every((b) => b.ended)).toBe(true);
  });

  it("只有思考没有正文时也保留思考卡片", async () => {
    stubFetch(sse([reasoningDelta("纯思考"), finishEvent]));

    await sendMessage(nextConvId(), "hi");

    const assistant = messages.value[1]!;
    expect(assistant.parts).toHaveLength(1);
    expect(assistant.parts[0]).toMatchObject({
      kind: "reasoning",
      text: "纯思考",
      ended: true,
    });
    expect(assistant.isStreaming).toBe(false);
  });

  it("sSE 行在网络分片中间断开仍能正确解析", async () => {
    const body =
      `data: ${JSON.stringify(textDelta("你"))}\n\n` +
      `data: ${JSON.stringify(textDelta("好"))}\n\n` +
      `data: ${JSON.stringify(finishEvent)}\n\n`;
    // 故意在 data 前缀中间和 JSON 中间切断
    const cut1 = body.indexOf("ata: ");
    const cut2 = body.indexOf('{"type"') + 4;
    stubFetch(
      sseResponse([
        body.slice(0, cut1),
        body.slice(cut1, cut2),
        body.slice(cut2),
      ]),
    );

    await sendMessage(nextConvId(), "hi");

    expect(messages.value[1]?.parts).toEqual([{ kind: "text", text: "你好" }]);
  });

  it("工具卡片完整生命周期：delta 生成 → start 运行 → 输出流 → result 完成", async () => {
    stubFetch(
      sse([
        toolCallDelta("call-1", '{"pa', "read_file"),
        toolCallStart("call-1", "read_file", "a.txt", '{"path":"a.txt"}'),
        toolOutputDelta("call-1", "stdout", "line1\n"),
        toolOutputDelta("call-1", "stdout", "line2\n"), // 同流合并
        toolOutputDelta("call-1", "stderr", "warn\n"), // 换流分段
        toolResult("call-1", "1. line1\n2. line2", false, {
          title: "a.txt",
          total_lines: 2,
        }),
        textDelta("done"),
        finishEvent,
      ]),
    );

    await sendMessage(nextConvId(), "read a.txt");

    const card = toolCards()[0]!;
    expect(card.name).toBe("read_file");
    expect(card.title).toBe("a.txt");
    expect(card.arguments).toBe('{"path":"a.txt"}'); // start 的 canonical 参数覆盖 delta 累积
    expect(card.status).toBe("done");
    expect(card.content).toBe("1. line1\n2. line2");
    expect(card.details).toEqual({ title: "a.txt", total_lines: 2 });
    expect(card.liveOutput.segments.map((s) => [s.stream, s.text])).toEqual([
      ["stdout", "line1\nline2\n"],
      ["stderr", "warn\n"],
    ]);
  });

  it("cancelled：generating 卡片被丢弃，running 卡片落为错误", async () => {
    stubFetch(
      sse([
        toolCallDelta("c-gen", "{}"), // 只到 delta：generating → 丢弃
        toolCallStart("c-run", "execute", "ls", "{}"), // 已派发：running → 错误
        toolCallStart("c-done", "read_file", "a.txt", "{}"),
        toolResult("c-done", "ok"), // 已有结果：done 保持
        cancelledEvent,
      ]),
    );

    await sendMessage(nextConvId(), "hi");

    const cards = toolCards().sort((a, b) => a.callId.localeCompare(b.callId));
    expect(cards.map((c) => c.callId)).toEqual(["c-done", "c-run"]);
    expect(cards[0]!.status).toBe("done");
    expect(cards[0]!.isError).toBe(false);
    expect(cards[1]!.status).toBe("done");
    expect(cards[1]!.isError).toBe(true);
    expect(messages.value.at(-1)?.isStreaming).toBe(false);
  });

  it("error 事件把错误文本合并进当前文本 part", async () => {
    stubFetch(sse([textDelta("部分回答"), errorEvent("LLM error: boom")]));

    await sendMessage(nextConvId(), "hi");

    // 尾部已是文本 part 时错误信息以空行分隔追加（与渲染行为一致）
    expect(messages.value[1]?.parts).toEqual([
      { kind: "text", text: "部分回答\n\nError: LLM error: boom" },
    ]);
  });

  it("hTTP 非 2xx 或网络失败时给出失败提示且不悬挂流式态", async () => {
    stubFetch(new Response("boom", { status: 500 }));
    await sendMessage(nextConvId(), "hi");
    expect(messages.value[1]?.parts).toEqual([
      { kind: "text", text: "Failed to send message." },
    ]);
    expect(sending.value).toBe(false);

    // 流式消息 id 取自 Date.now()：隔几毫秒避免与上一次 send 撞 id
    await new Promise((r) => setTimeout(r, 5));
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("network")));
    await sendMessage(nextConvId(), "hi");
    expect(messages.value.at(-1)?.parts).toEqual([
      { kind: "text", text: "Failed to send message." },
    ]);
    expect(sending.value).toBe(false);
  });

  it("空白内容直接忽略，不发起请求", async () => {
    const fetchMock = stubFetch();
    await sendMessage(nextConvId(), "   ");
    expect(fetchMock).not.toHaveBeenCalled();
    expect(messages.value).toHaveLength(0);
  });

  it("live output 超过尾部上限时裁掉最旧内容并计数", async () => {
    // 40KB stdout 输出 > LIVE_OUTPUT_MAX_CHARS(32768)
    const chunk = "x".repeat(1024);
    const events = [
      toolCallStart("c1", "execute", "cat big", "{}"),
      ...Array.from({ length: 40 }).fill(
        toolOutputDelta("c1", "stdout", chunk),
      ),
      toolResult("c1", "final"),
      finishEvent,
    ];
    stubFetch(sse(events));

    await sendMessage(nextConvId(), "hi");

    const card = toolCards()[0]!;
    const kept = card.liveOutput.segments.reduce(
      (n, s) => n + s.text.length,
      0,
    );
    expect(kept).toBe(32768);
    expect(card.liveOutput.droppedChars).toBe(40 * 1024 - 32768);
  });
});

// ---------------------------------------------------------------------------
// 中断恢复（resumable / resumeTurn）
// ---------------------------------------------------------------------------

describe("中断恢复", () => {
  function stubFetch(...responses: Response[]) {
    const fetchMock = vi.fn();
    for (const r of responses) fetchMock.mockResolvedValueOnce(r);
    vi.stubGlobal("fetch", fetchMock);
    return fetchMock;
  }

  /** 带悬空 ToolCall 的历史（后端崩溃后留下的形状）。 */
  function danglingHistory() {
    return [
      storedMessage("user", '[{"Text":"读一下"}]'),
      storedMessage(
        "assistant",
        String.raw`[{"ToolCall":{"id":"c1","name":"read_file","arguments":"{\"path\":\"a.txt\"}"}}]`,
      ),
    ];
  }

  it("resumable：悬空卡片或最后一条是 user 时为真", async () => {
    // 完整问答：不可恢复
    GET.mockResolvedValue({
      data: [
        storedMessage("user", '[{"Text":"问"}]'),
        storedMessage("assistant", '[{"Text":"答"}]'),
      ],
    } as never);
    await fetchMessages(nextConvId());
    expect(resumable.value).toBe(false);

    // 悬空工具卡片：可恢复
    GET.mockResolvedValue({ data: danglingHistory() } as never);
    await fetchMessages(nextConvId());
    expect(resumable.value).toBe(true);

    // 最后一条是 user（发了没得到回答）：可恢复
    GET.mockResolvedValue({
      data: [
        storedMessage("user", '[{"Text":"问"}]'),
        storedMessage("assistant", '[{"Text":"答"}]'),
        storedMessage("user", '[{"Text":"追问"}]'),
      ],
    } as never);
    await fetchMessages(nextConvId());
    expect(resumable.value).toBe(true);

    // 空会话：不可恢复
    GET.mockResolvedValue({ data: [] } as never);
    await fetchMessages(nextConvId());
    expect(resumable.value).toBe(false);
  });

  it("resumeTurn：先拉取占位行，再消费 SSE，终态静默刷新", async () => {
    const fetchMock = stubFetch(sse([textDelta("续答"), finishEvent]));

    // 第一次 GET（恢复后拿占位 tool 行）：悬空历史；
    // 第二次 GET（终态刷新）：后端已持久化 assistant 回复
    GET.mockResolvedValueOnce({
      data: danglingHistory(),
    } as never);
    GET.mockResolvedValueOnce({
      data: [
        ...danglingHistory(),
        storedMessage(
          "tool",
          '[{"ToolResult":{"call_id":"c1","content":"工具执行被中断（后端重启），工作区状态未知。","is_error":true}}]',
        ),
        storedMessage("assistant", '[{"Text":"续答"}]'),
      ],
    } as never);

    const conv = nextConvId();
    await resumeTurn(conv);

    // POST 到 resume 端点，不携带请求体
    const [url, init] = fetchMock.mock.calls[0]!;
    expect(url).toBe(`/api/conversations/${conv}/resume`);
    expect(init).toMatchObject({
      method: "POST",
      signal: expect.any(AbortSignal),
    });

    // 两次 GET：占位行拉取 + 终态刷新
    expect(GET).toHaveBeenCalledTimes(2);

    // 终态来自服务端刷新：占位结果回填了悬空卡片，回复也在，不再可恢复
    expect(GET.mock.calls[1]?.[0]).toBe("/api/conversations/{id}/messages");
    const card = toolCards()[0]!;
    expect(card.status).toBe("done");
    expect(card.isError).toBe(true);
    expect(card.content).toContain("工具执行被中断");
    expect(messages.value.at(-1)?.parts).toEqual([
      { kind: "text", text: "续答" },
    ]);
    // 终态消息来自历史刷新，不在流式态
    expect(messages.value.at(-1)?.isStreaming).toBeFalsy();
    expect(resumable.value).toBe(false);
    expect(sending.value).toBe(false);
  });

  it("sendMessage：发送前有中断卡片时，结束后刷新历史回填占位结果", async () => {
    const conv = nextConvId();
    // 初始视图带悬空工具卡片（后端 send 时会顺带修复并落占位行）
    GET.mockResolvedValueOnce({ data: danglingHistory() } as never);
    await fetchMessages(conv);
    expect(toolCards()[0]!.status).toBe("interrupted");

    stubFetch(sse([textDelta("新答"), finishEvent]));
    // 终态刷新：占位 tool 行与新的问答都已持久化
    GET.mockResolvedValueOnce({
      data: [
        ...danglingHistory(),
        storedMessage(
          "tool",
          '[{"ToolResult":{"call_id":"c1","content":"工具执行被中断（后端重启），工作区状态未知。","is_error":true}}]',
        ),
        storedMessage("user", '[{"Text":"继续"}]'),
        storedMessage("assistant", '[{"Text":"新答"}]'),
      ],
    } as never);

    await sendMessage(conv, "继续");

    // 初始加载 + 终态刷新共两次 GET；中断卡片被占位结果回填，不再可恢复
    expect(GET).toHaveBeenCalledTimes(2);
    const card = toolCards()[0]!;
    expect(card.status).toBe("done");
    expect(card.isError).toBe(true);
    expect(resumable.value).toBe(false);
  });

  it("resumeTurn：流式进行中切走会话，收尾刷新不覆盖当前视图", async () => {
    // 读取先挂起，切换会话会主动取消 reader 并收尾。
    const stream = new ReadableStream<Uint8Array>();
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValueOnce(
        new Response(stream, {
          status: 200,
          headers: { "content-type": "text/event-stream" },
        }),
      ),
    );

    const convA = nextConvId();
    // resume 成功后的占位行拉取
    GET.mockResolvedValueOnce({ data: danglingHistory() } as never);
    const resumePromise = resumeTurn(convA);
    // 占位行拉取完成、流式消息已建立（读取挂起中）
    await vi.waitFor(() =>
      expect(messages.value.some((m) => m.id.startsWith("resumed-"))).toBe(
        true,
      ),
    );

    // 切换到会话 B
    const convB = nextConvId();
    GET.mockResolvedValueOnce({
      data: [storedMessage("user", '[{"Text":"B会话"}]')],
    } as never);
    await fetchMessages(convB);

    // 切换会话会取消 reader；无需等旧网络流自行结束。
    await resumePromise;

    // 视图仍是 B：收尾刷新被会话守卫拦下（A 的重拉由路由 watch 负责）
    expect(messages.value.map((m) => m.role)).toEqual(["user"]);
    expect(messages.value[0]!.parts).toEqual([{ kind: "text", text: "B会话" }]);
    expect(GET).toHaveBeenCalledTimes(2); // A 占位行 + B 切换加载
    expect(sending.value).toBe(false);
  });

  it("resumeTurn：409 显示失败反馈并释放 sending", async () => {
    stubFetch(new Response("conflict", { status: 409 }));
    GET.mockResolvedValue({ data: danglingHistory() } as never);

    // 先建立视图（恢复按钮只在会话展示时出现），否则收尾刷新的会话
    // 守卫不生效
    const conv = nextConvId();
    await fetchMessages(conv);
    await resumeTurn(conv);

    // 失败反馈不被历史刷新擦掉，原中断卡仍可恢复。
    expect(GET).toHaveBeenCalledTimes(1);
    expect(messages.value.map((m) => m.role)).toEqual([
      "user",
      "assistant",
      "assistant",
    ]);
    expect(JSON.stringify(messages.value)).toContain("Failed to resume turn.");
    expect(toolCards()[0]!.status).toBe("interrupted");
    expect(sending.value).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// 运行中重连重放（attachRun：前端刷新后重接运行中的 run）
// ---------------------------------------------------------------------------

describe("attachRun 重连重放", () => {
  function stubFetch(...responses: Response[]) {
    const fetchMock = vi.fn();
    for (const r of responses) fetchMock.mockResolvedValueOnce(r);
    vi.stubGlobal("fetch", fetchMock);
    return fetchMock;
  }

  /** 可暂停的 SSE（controller 手动放行），模拟重连后仍在增长的流。 */
  function pausableSse() {
    const encoder = new TextEncoder();
    let streamController!: ReadableStreamDefaultController<Uint8Array>;
    const stream = new ReadableStream<Uint8Array>({
      start(c) {
        streamController = c;
      },
    });
    const fetchMock = vi.fn().mockResolvedValueOnce(
      new Response(stream, {
        status: 200,
        headers: { "content-type": "text/event-stream" },
      }),
    );
    vi.stubGlobal("fetch", fetchMock);
    return {
      fetchMock,
      send: (...events: unknown[]) =>
        streamController.enqueue(
          encoder.encode(
            `${events.map((e) => `data: ${JSON.stringify(e)}`).join("\n\n")}\n\n`,
          ),
        ),
      end: () => streamController.close(),
    };
  }

  /** GET 队列：初始历史 → /run 探测 →（运行中）approvals 快照 →（终态）静默刷新。 */
  function stubGetQueue(...results: unknown[]) {
    for (const r of results) GET.mockResolvedValueOnce(r as never);
  }

  /** 运行中 run 的 approvals 快照槽位（空 pending）。 */
  const emptyApprovals = { data: [] };

  /** 终态刷新失败：保留重建视图以便断言重放结果（loadMessages 失败不动视图）。 */
  const failedRefresh = { data: undefined, error: {} };

  it("主路径：run_meta 锚点截断 → 事件重放重建 → 终态守卫刷新", async () => {
    const conv = nextConvId();
    const startedAt = Date.now() - 65_000;
    // 刷新后从 DB 拉到的历史：run 的 user 消息（u2）之后已有轮次边界
    // 落库的第一轮 assistant（a2）——属于 run 之内，重放前必须丢弃。
    stubGetQueue(
      {
        data: [
          storedMessage("user", '[{"Text":"第一问"}]', { id: "u1" }),
          storedMessage("assistant", '[{"Text":"旧答"}]', { id: "a1" }),
          storedMessage("user", '[{"Text":"run 的问题"}]', {
            id: "u2",
            started_at: new Date(startedAt).toISOString(),
          }),
          storedMessage(
            "assistant",
            '[{"ToolCall":{"id":"c1","name":"read_file","arguments":"{}"}}]',
            { id: "a2" },
          ),
        ],
      },
      { data: { running: true } },
      emptyApprovals,
      failedRefresh,
    );
    const sse = pausableSse();

    await fetchMessages(conv);
    const attachPromise = attachRun(conv);
    // 首个事件（run_meta）到达：截断已发生、重放流式消息已建立
    sse.send(runMeta("u2"));
    await vi.waitFor(() =>
      expect(messages.value.some((m) => RE_reattachedId.test(m.id))).toBe(true),
    );
    expect(messages.value.map((m) => m.id)).toEqual([
      "u1",
      "a1",
      "u2",
      expect.stringMatching(RE_reattachedId),
    ]);
    expect(messages.value.at(-1)?.startedAt).toBe(startedAt);

    // 重放 delta 经现有 reducer 重建（与 live 流同一套逻辑），直至 finish
    sse.send(
      reasoningDelta("想想"),
      textDelta("重建"),
      textDelta("答案"),
      finishEvent,
    );
    sse.end();
    await attachPromise;

    const rebuilt = messages.value[3]!;
    expect(rebuilt.startedAt).toBe(startedAt);
    expect(rebuilt.finishedAt).toBeGreaterThanOrEqual(startedAt + 65_000);
    expect(rebuilt.parts).toEqual([
      {
        kind: "reasoning",
        text: "想想",
        startedAt: expect.any(Number),
        durationMs: expect.any(Number),
        ended: true,
      },
      { kind: "text", text: "重建答案" },
    ]);
    expect(rebuilt.isStreaming).toBe(false);
    expect(sending.value).toBe(false);
    // 订阅打到 events 端点；GET 轨迹：初始加载 → /run 探测 → approvals
    // 快照 → 终态刷新
    expect(sse.fetchMock.mock.calls[0]?.[0]).toBe(
      `/api/conversations/${conv}/events`,
    );
    expect(GET).toHaveBeenCalledTimes(4);
    expect(GET.mock.calls[1]?.[0]).toBe("/api/conversations/{id}/run");
    expect(GET.mock.calls[2]?.[0]).toBe("/api/conversations/{id}/approvals");
    expect(GET.mock.calls[3]?.[0]).toBe("/api/conversations/{id}/messages");
  });

  it("空闲会话：只静默刷新收敛，不订阅事件流、不悬挂", async () => {
    const conv = nextConvId();
    stubGetQueue(
      { data: [storedMessage("user", '[{"Text":"问"}]', { id: "u1" })] },
      { data: { running: false } },
      emptyApprovals,
      {
        data: [
          storedMessage("user", '[{"Text":"问"}]', { id: "u1" }),
          storedMessage("assistant", '[{"Text":"答"}]'),
        ],
      },
    );
    const fetchMock = stubFetch();

    await fetchMessages(conv);
    await attachRun(conv);

    expect(fetchMock).not.toHaveBeenCalled();
    expect(GET).toHaveBeenCalledTimes(4); // 初始 + 探测 + 审批快照 + 静默刷新
    expect(messages.value.at(-1)?.parts).toEqual([
      { kind: "text", text: "答" },
    ]);
    expect(sending.value).toBe(false);
  });

  it("缺少执行边界时保留已加载历史并结束订阅", async () => {
    const conv = nextConvId();
    // 旧协议无法证明重放起点：保留 DB 中已加载的 assistant。
    stubGetQueue(
      {
        data: [
          storedMessage("user", '[{"Text":"恢复的请求"}]', { id: "u1" }),
          storedMessage("assistant", '[{"Text":"中断的半截回答"}]', {
            id: "a1",
          }),
        ],
      },
      { data: { running: true } },
      emptyApprovals,
      failedRefresh,
    );
    stubFetch(sse([textDelta("续答"), finishEvent]));
    await fetchMessages(conv);
    await attachRun(conv);

    expect(messages.value.slice(0, 2).map((m) => m.id)).toEqual(["u1", "a1"]);
    expect(JSON.stringify(messages.value)).toContain("中断的半截回答");
    expect(JSON.stringify(messages.value)).not.toContain("续答");
    expect(JSON.stringify(messages.value)).toContain("执行边界");
    expect(sending.value).toBe(false);
  });

  it("探测与订阅之间 run 结束（events 404）：静默刷新收敛，不追加重放消息", async () => {
    const conv = nextConvId();
    stubGetQueue(
      { data: [storedMessage("user", '[{"Text":"问"}]', { id: "u1" })] },
      { data: { running: true } },
      emptyApprovals,
      {
        data: [
          storedMessage("user", '[{"Text":"问"}]', { id: "u1" }),
          storedMessage("assistant", '[{"Text":"答"}]'),
        ],
      },
    );
    stubFetch(new Response("gone", { status: 404 }));
    await fetchMessages(conv);
    await attachRun(conv);

    expect(messages.value.map((m) => m.role)).toEqual(["user", "assistant"]);
    expect(messages.value.some((m) => RE_reattachedId.test(m.id))).toBe(false);
    expect(GET).toHaveBeenCalledTimes(4);
    expect(sending.value).toBe(false);
  });

  it("流 EOF 无终态事件：finalize 收敛，不悬挂流式态", async () => {
    const conv = nextConvId();
    stubGetQueue(
      {
        data: [
          storedMessage("user", '[{"Text":"问"}]', { id: "u1" }),
          storedMessage("user", '[{"Text":"run 的问题"}]', { id: "u2" }),
        ],
      },
      { data: { running: true } },
      emptyApprovals,
      failedRefresh,
    );
    // 中途 run 结束（hub drop ⟹ 流关闭）且最后一截没带终态事件
    stubFetch(sse([runMeta("u2"), textDelta("部分")]));
    await fetchMessages(conv);
    await attachRun(conv);

    const rebuilt = messages.value[2]!;
    expect(rebuilt.id).toMatch(RE_reattachedId);
    expect(rebuilt.parts).toEqual([{ kind: "text", text: "部分" }]);
    expect(rebuilt.isStreaming).toBe(false);
    expect(sending.value).toBe(false);
  });

  it("attach 进行中切走会话：事件不落在别的会话视图上，收尾刷新被守卫拦下", async () => {
    const convA = nextConvId();
    stubGetQueue(
      {
        data: [
          storedMessage("user", '[{"Text":"A 的问题"}]', { id: "u1" }),
          storedMessage("user", '[{"Text":"run 的问题"}]', { id: "u2" }),
        ],
      },
      { data: { running: true } },
      emptyApprovals,
    );
    const sse = pausableSse();

    await fetchMessages(convA);
    const attachPromise = attachRun(convA);
    sse.send(runMeta("u2"));
    await vi.waitFor(() =>
      expect(messages.value.some((m) => RE_reattachedId.test(m.id))).toBe(true),
    );

    // 切换到会话 B（GET#4：B 的加载）
    const convB = nextConvId();
    stubGetQueue({
      data: [storedMessage("user", '[{"Text":"B会话"}]', { id: "b1" })],
    });
    await fetchMessages(convB);

    // reader 取消后旧流自行收尾，不能刷新 B 或清除它的忙碌状态。
    expect(sse.fetchMock.mock.calls[0]?.[1].signal.aborted).toBe(true);
    await attachPromise;

    expect(messages.value.map((m) => m.id)).toEqual(["b1"]);
    expect(GET).toHaveBeenCalledTimes(4); // A 初始 + A 探测 + A 快照 + B 切换（A 收尾刷新被拦）
    expect(sending.value).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// title_updated：会话元数据事件 → 监听器派发（不进消息 reducer）
// ---------------------------------------------------------------------------

describe("title_updated 事件派发", () => {
  const titleUpdated = (title: string) => ({
    type: "title_updated",
    data: { title },
  });

  it("在 SSE 消费层派发给监听器，携带会话 id 与新标题", async () => {
    vi.stubGlobal(
      "fetch",
      vi
        .fn()
        .mockResolvedValue(
          sse([textDelta("ok"), titleUpdated("修复登录按钮"), finishEvent]),
        ),
    );

    const seen: Array<[string, string]> = [];
    const unsubscribe = onTitleUpdated((conversationId, title) => {
      seen.push([conversationId, title]);
    });

    const convId = nextConvId();
    await sendMessage(convId, "hi");

    expect(seen).toEqual([[convId, "修复登录按钮"]]);
    // 元数据事件不产生消息内容
    expect(
      messages.value.some((m) =>
        m.parts.some(
          (p) => p.kind === "text" && p.text.includes("修复登录按钮"),
        ),
      ),
    ).toBe(false);

    unsubscribe();
  });

  it("取消订阅后不再收到事件；未命中监听器静默", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(sse([titleUpdated("无人在听"), finishEvent])),
    );

    const seen: string[] = [];
    const unsubscribe = onTitleUpdated((_id, title) => seen.push(title));
    unsubscribe();

    await sendMessage(nextConvId(), "hi");
    expect(seen).toEqual([]);
  });
});

it("视图停留在其他会话时派发不受影响（侧栏更新不依赖当前视图）", async () => {
  // 先加载会话 B，让 activeConversationId 指向 B。
  GET.mockResolvedValueOnce({
    data: [storedMessage("user", '[{"Text":"B会话"}]', { id: "b1" })],
    error: undefined,
  } as never);
  await fetchMessages("conv-view-b");

  // A 上发消息（不切视图），流中带来 A 的 title_updated。
  vi.stubGlobal(
    "fetch",
    vi
      .fn()
      .mockResolvedValue(
        sse([
          { type: "title_updated", data: { title: "A 的新标题" } },
          finishEvent,
        ]),
      ),
  );
  const seen: Array<[string, string]> = [];
  const unsubscribe = onTitleUpdated((conversationId, title) => {
    seen.push([conversationId, title]);
  });

  await sendMessage("conv-a", "hi");

  expect(seen).toEqual([["conv-a", "A 的新标题"]]);
  // sendMessage 将活动会话切到 A；流结束后刷新持久化消息与 fork revision。
  expect(GET).toHaveBeenCalledTimes(2);

  unsubscribe();
});

const { compactConversation } = useChat();

function compactionParts(): DeepReadonly<CompactionPart>[] {
  return messages.value.flatMap((m) =>
    m.parts.filter(
      (p): p is DeepReadonly<CompactionPart> => p.kind === "compaction",
    ),
  );
}

describe("微压缩 microcompact", () => {
  beforeEach(() => {
    POST.mockReset();
  });

  it("microcompacted 事件就地折叠历史工具卡片（跨消息，幂等重放）", async () => {
    const conv = nextConvId();
    // 后端语义：microcompact 在 turn-top 触发且不碰最后一轮，目标卡片几乎
    // 总在历史消息里——先灌入历史，本轮流只带事件。
    GET.mockResolvedValue({
      data: [
        storedMessage("user", '[{"Text":"旧问题"}]'),
        storedMessage(
          "assistant",
          JSON.stringify([
            { ToolCall: { id: "c1", name: "read_file", arguments: "{}" } },
          ]),
        ),
        storedMessage(
          "tool",
          JSON.stringify([
            {
              ToolResult: {
                call_id: "c1",
                content: "很长的文件内容……",
                is_error: false,
              },
            },
          ]),
        ),
      ],
    } as never);
    await fetchMessages(conv);

    const fetchMock = vi.fn(() =>
      Promise.resolve(
        sse([
          {
            type: "microcompacted",
            data: {
              triggered_by: "waterline",
              replacements: [{ call_id: "c1", copy_message_id: "copy-1" }],
              est_tokens_saved: 4200,
              est_context_tokens: 5800,
            },
          },
          // 重放同一事件：值不变，幂等。
          {
            type: "microcompacted",
            data: {
              triggered_by: "waterline",
              replacements: [{ call_id: "c1", copy_message_id: "copy-1" }],
              est_tokens_saved: 4200,
              est_context_tokens: 5800,
            },
          },
          textDelta("继续"),
          finishEvent,
        ]),
      ),
    );
    vi.stubGlobal("fetch", fetchMock);

    await sendMessage(conv, "hi");

    // 历史消息里的卡片被就地折叠（响应式路径），流式新消息不受影响。
    const historyCard =
      (messages.value[1]?.parts[0] as {
        content: string | null;
        cleared: boolean;
        clearedCopyMessageId: string | null;
      }) ?? null;
    expect(historyCard).not.toBeNull();
    expect(historyCard.content).toBe("[Old tool result content cleared]");
    expect(historyCard.cleared).toBe(true);
    expect(historyCard.clearedCopyMessageId).toBe("copy-1");
    const streaming = messages.value.at(-1)!;
    expect(streaming.parts.some((p) => p.kind === "tool_call")).toBe(false);
  });

  it("stored 副本行（details kind=microcompact_cleared）回放为已清除卡片", async () => {
    GET.mockResolvedValue({
      data: [
        storedMessage(
          "assistant",
          JSON.stringify([
            {
              ToolCall: { id: "c1", name: "read_file", arguments: "{}" },
            },
          ]),
        ),
        storedMessage(
          "tool",
          JSON.stringify([
            {
              ToolResult: {
                call_id: "c1",
                content: "[Old tool result content cleared]",
                is_error: false,
              },
            },
          ]),
          {
            id: "copy-row-1",
            details: { kind: "microcompact_cleared" },
          },
        ),
      ],
    } as never);

    await fetchMessages(nextConvId());

    const cards: unknown[] = [];
    for (const m of messages.value) {
      for (const p of m.parts) {
        if (p.kind === "tool_call") cards.push(p);
      }
    }
    expect(cards).toHaveLength(1);
    const card = cards[0] as {
      content: string | null;
      cleared: boolean;
      clearedCopyMessageId: string | null;
    };
    expect(card.cleared).toBe(true);
    expect(card.clearedCopyMessageId).toBe("copy-row-1");
    expect(card.content).toBe("[Old tool result content cleared]");
  });
});

describe("上下文压缩", () => {
  beforeEach(() => {
    POST.mockReset();
  });

  it("stored 标记行渲染为 compaction 分隔条而非用户气泡", async () => {
    GET.mockResolvedValue({
      data: [
        storedMessage("user", '[{"Text":"旧问题"}]'),
        storedMessage("assistant", '[{"Text":"旧回答"}]'),
        storedMessage(
          "user",
          JSON.stringify([
            {
              Text: "[Context compaction] 前缀\n\n<summary>\n做了 A 和 B\n</summary>\n\n后缀",
            },
          ]),
          {
            details: {
              kind: "compaction",
              compaction: {
                triggered_by: "auto",
                compacted_message_count: 2,
                estimated_tokens_before: 1000,
                estimated_tokens_after: 200,
              },
            },
          },
        ),
        storedMessage("user", '[{"Text":"新问题"}]'),
      ],
    } as never);

    await fetchMessages(nextConvId());

    // 标记行不是 user 气泡，而是独立角色的分隔条；摘要剥掉框架文本。
    expect(messages.value.map((m) => m.role)).toEqual([
      "user",
      "assistant",
      "compaction",
      "user",
    ]);
    expect(messages.value[2]?.parts[0]).toEqual({
      kind: "compaction",
      summary: "做了 A 和 B",
      triggeredBy: "auto",
      compactedMessages: 2,
      estTokensBefore: 1000,
      estTokensAfter: 200,
    });
  });

  it("compacted 事件当场插入分隔条并驱动 usage 指示器", async () => {
    const fetchMock = vi.fn(() =>
      Promise.resolve(
        sse([
          { type: "compacting", data: { triggered_by: "auto" } },
          {
            type: "compacted",
            data: {
              summary_message_id: "m1",
              triggered_by: "auto",
              compacted_messages: 5,
              est_tokens_before: 900,
              est_tokens_after: 100,
              summary: "摘好了",
              context_window_tokens: 2000,
            },
          },
          {
            type: "usage",
            data: {
              input_tokens: 120,
              output_tokens: 8,
              reasoning_tokens: null,
              context_window_tokens: 2000,
            },
          },
          textDelta("继续"),
          finishEvent,
        ]),
      ),
    );
    vi.stubGlobal("fetch", fetchMock);

    await sendMessage(nextConvId(), "hi");

    // 分隔条先于本轮文本落进流式消息的 parts。
    const parts = compactionParts();
    expect(parts).toHaveLength(1);
    expect(parts[0]).toMatchObject({ summary: "摘好了", triggeredBy: "auto" });
    const last = messages.value.at(-1)!;
    expect(last.parts[0]?.kind).toBe("compaction");
    expect(last.parts[1]).toEqual({ kind: "text", text: "继续" });

    const { inputTokens, windowTokens, compacting } = useContextUsage();
    expect(inputTokens.value).toBe(120);
    expect(windowTokens.value).toBe(2000);
    expect(compacting.value).toBe(false);
  });

  it("手动 compact 调端点、成功后重拉历史", async () => {
    const conv = nextConvId();
    // 视图先落在该会话上（activeConversationId 守卫：切走后不重拉）。
    GET.mockResolvedValueOnce({
      data: [storedMessage("user", '[{"Text":"旧问题"}]')],
    } as never);
    await fetchMessages(conv);

    POST.mockResolvedValue({
      data: {
        summary_message_id: "m1",
        compacted_messages: 3,
        est_tokens_before: 900,
        est_tokens_after: 100,
        summary: "手动摘要",
      },
    } as never);
    GET.mockResolvedValue({
      data: [
        storedMessage(
          "user",
          JSON.stringify([
            { Text: "[Context compaction]\n<summary>\n手动摘要\n</summary>" },
          ]),
          {
            details: {
              kind: "compaction",
              compaction: {
                triggered_by: "manual",
                compacted_message_count: 3,
                estimated_tokens_before: 900,
                estimated_tokens_after: 100,
              },
            },
          },
        ),
      ],
    } as never);

    await compactConversation(conv);

    expect(POST).toHaveBeenCalledWith(
      "/api/conversations/{id}/compact",
      expect.objectContaining({ params: { path: { id: conv } } }),
    );
    // 重拉后由 stored 路径接管同一展示。
    expect(messages.value.map((m) => m.role)).toEqual(["compaction"]);
    expect(compactionParts()[0]).toMatchObject({
      summary: "手动摘要",
      triggeredBy: "manual",
    });
  });
});

// ---------------------------------------------------------------------------
// createChatSession：实例化隔离（子代理 panel 的会话状态不碰单例/全局）
// ---------------------------------------------------------------------------

describe("createChatSession 实例化", () => {
  function stubFetch(...responses: Response[]) {
    const fetchMock = vi.fn();
    for (const r of responses) fetchMock.mockResolvedValueOnce(r);
    vi.stubGlobal("fetch", fetchMock);
    return fetchMock;
  }

  it("两个实例状态隔离：流入 A 的事件不出现在 B（单例也不受影响）", async () => {
    const a = createChatSession();
    const b = createChatSession();
    const singletonBefore = messages.value.length;

    stubFetch(
      sse([
        textDelta("A 的回答"),
        toolCallStart("a-call", "read_file", "a.txt", "{}"),
        toolResult("a-call", "A 的工具结果"),
        finishEvent,
      ]),
    );
    await a.sendMessage("conv-a", "hi");

    expect(a.messages.value).toHaveLength(2);
    expect(a.messages.value[1]?.parts).toEqual([
      { kind: "text", text: "A 的回答" },
      expect.objectContaining({ kind: "tool_call", callId: "a-call" }),
    ]);
    // B 完全干净；useChat 单例也不受影响
    expect(b.messages.value).toHaveLength(0);
    expect(b.sending.value).toBe(false);
    expect(messages.value.length).toBe(singletonBefore);
  });

  it("contextUsage:false 不写全局上下文环；实例本地快照与压缩分隔条仍工作", async () => {
    clearContextUsage();
    const session = createChatSession({ contextUsage: false });

    stubFetch(
      sse([
        { type: "compacting", data: { triggered_by: "auto" } },
        {
          type: "compacted",
          data: {
            summary_message_id: "m1",
            triggered_by: "auto",
            compacted_messages: 4,
            est_tokens_before: 800,
            est_tokens_after: 100,
            summary: "实例内摘要",
            context_window_tokens: 2000,
          },
        },
        {
          type: "usage",
          data: {
            input_tokens: 150,
            output_tokens: 8,
            reasoning_tokens: null,
            context_window_tokens: 2000,
          },
        },
        textDelta("子代理回答"),
        finishEvent,
      ]),
    );
    await session.sendMessage("conv-sub", "hi");

    // 全局单例未被污染（子 usage 不进父上下文环）
    const { inputTokens, windowTokens, compacting } = useContextUsage();
    expect(inputTokens.value).toBeNull();
    expect(windowTokens.value).toBeNull();
    expect(compacting.value).toBe(false);

    // 实例本地快照记录了 usage；压缩分隔条照常渲染
    expect(session.usage.inputTokens.value).toBe(150);
    expect(session.usage.windowTokens.value).toBe(2000);
    expect(session.usage.compacting.value).toBe(false);
    const last = session.messages.value.at(-1)!;
    expect(last.parts[0]?.kind).toBe("compaction");
    expect(last.parts[1]).toEqual({ kind: "text", text: "子代理回答" });
  });

  it("默认实例照旧写全局上下文环（主面板行为不变）", async () => {
    clearContextUsage();
    const session = createChatSession();
    stubFetch(
      sse([
        {
          type: "usage",
          data: { input_tokens: 42, context_window_tokens: 1000 },
        },
        finishEvent,
      ]),
    );
    await session.sendMessage("conv-main", "hi");

    const { inputTokens } = useContextUsage();
    expect(inputTokens.value).toBe(42);
    expect(session.usage.inputTokens.value).toBe(42);
  });
});

// ---------------------------------------------------------------------------
// attachSubagent：子代理面板挂载（历史 + 活动 run 探测/重放）
// ---------------------------------------------------------------------------

describe("attachSubagent", () => {
  function stubFetch(...responses: Response[]) {
    const fetchMock = vi.fn();
    for (const r of responses) fetchMock.mockResolvedValueOnce(r);
    vi.stubGlobal("fetch", fetchMock);
    return fetchMock;
  }

  /** 子会话历史（委派 user 消息 + 一轮 assistant）。 */
  function subagentHistory() {
    return [
      storedMessage("user", '[{"Text":"检查 useChat 的重连路径"}]', {
        id: "u1",
      }),
      storedMessage("assistant", '[{"Text":"第一轮半截回答"}]', { id: "a1" }),
    ];
  }

  it("历史 + 404（无活动 run）：仅历史视图", async () => {
    const session = createChatSession({ contextUsage: false });
    const fetchMock = stubFetch(new Response("gone", { status: 404 }));
    GET.mockResolvedValue({
      data: subagentHistory(),
      error: undefined,
    } as never);

    await session.attachSubagent("agent-1");

    // 订阅打到子代理 events 端点；历史经 messages 端点拉取（首拉 + 收尾刷新）
    expect(fetchMock.mock.calls[0]?.[0]).toBe("/api/subagents/agent-1/events");
    expect(GET).toHaveBeenNthCalledWith(
      1,
      "/api/subagents/{agentId}/messages",
      { params: { path: { agentId: "agent-1" } } },
    );
    expect(session.messages.value.map((m) => m.id)).toEqual(["u1", "a1"]);
    expect(session.sending.value).toBe(false);
  });

  it("历史 + SSE 重放：run_meta 锚点截断后重放重建（与 attachRun 同语义）", async () => {
    const session = createChatSession({ contextUsage: false });
    // 重放会看到 a1（run 之内已落库的轮次）——重放前必须按锚点丢弃。
    // 收尾刷新按失败处理（attachRun 测试同款），保留重建视图以便断言。
    // as never：vi.mocked 的重载会把返回类型钉在首个路径上，子代理历史
    // 端点的 MessageDto[] 形状与其不兼容（其余 mock 同款处理）。
    GET.mockResolvedValueOnce({
      data: subagentHistory(),
      error: undefined,
    } as never);
    GET.mockResolvedValue({ data: undefined, error: {} } as never);
    const fetchMock = stubFetch(
      sse([runMeta("u1"), textDelta("重建的"), textDelta("回答"), finishEvent]),
    );

    await session.attachSubagent("agent-1");

    expect(session.messages.value.map((m) => m.id)).toEqual([
      "u1",
      expect.stringMatching(RE_reattachedId),
    ]);
    const rebuilt = session.messages.value[1]!;
    expect(rebuilt.parts).toEqual([{ kind: "text", text: "重建的回答" }]);
    expect(rebuilt.isStreaming).toBe(false);
    expect(session.sending.value).toBe(false);
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it("reattaches after incomplete EOF and keeps showing live child output without duplicates", async () => {
    const session = createChatSession({ contextUsage: false });
    GET.mockResolvedValueOnce({ data: subagentHistory() } as never);
    GET.mockResolvedValue({ error: {} } as never);
    const encoder = new TextEncoder();
    let controller!: ReadableStreamDefaultController<Uint8Array>;
    const live = new ReadableStream<Uint8Array>({
      start(c) {
        controller = c;
        c.enqueue(
          encoder.encode(
            `data: ${JSON.stringify(runMeta("u1"))}\n\n` +
              `data: ${JSON.stringify(textDelta("partial and continuing"))}\n\n`,
          ),
        );
      },
    });
    const fetchMock = stubFetch(
      sse([runMeta("u1"), textDelta("partial")]),
      new Response(live, { status: 200 }),
    );

    const attached = session.attachSubagent("agent-1");
    try {
      await vi.waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));
      await vi.waitFor(() =>
        expect(session.messages.value[1]?.parts).toEqual([
          { kind: "text", text: "partial and continuing" },
        ]),
      );
      expect(session.sending.value).toBe(true);
      expect(session.messages.value).toHaveLength(2);
      expect(fetchMock.mock.calls[1]?.[0]).toBe(
        "/api/subagents/agent-1/events",
      );
    } finally {
      controller.enqueue(
        encoder.encode(`data: ${JSON.stringify(finishEvent)}\n\n`),
      );
      controller.close();
      await attached;
      session.dispose();
    }
    expect(fetchMock).toHaveBeenCalledTimes(2);
    expect(session.sending.value).toBe(false);
  });

  it("reattaches after a stream read fails", async () => {
    const session = createChatSession({ contextUsage: false });
    GET.mockResolvedValueOnce({ data: subagentHistory() } as never);
    GET.mockResolvedValue({ error: {} } as never);
    const broken = new ReadableStream<Uint8Array>({
      start(controller) {
        controller.error(new TypeError("connection reset"));
      },
    });
    const fetchMock = stubFetch(
      new Response(broken, { status: 200 }),
      sse([runMeta("u1"), textDelta("recovered child output"), finishEvent]),
    );

    await session.attachSubagent("agent-1");

    expect(fetchMock).toHaveBeenCalledTimes(2);
    expect(session.messages.value[1]?.parts).toEqual([
      { kind: "text", text: "recovered child output" },
    ]);
    expect(session.sending.value).toBe(false);
    session.dispose();
  });

  it("bounds retries when child subscriptions repeatedly end without a terminal event", async () => {
    const session = createChatSession({ contextUsage: false });
    GET.mockResolvedValue({ data: subagentHistory() } as never);
    const fetchMock = stubFetch(
      sse([runMeta("u1"), textDelta("partial")]),
      sse([runMeta("u1"), textDelta("partial")]),
      sse([runMeta("u1"), textDelta("partial")]),
    );

    await session.attachSubagent("agent-1");

    expect(fetchMock).toHaveBeenCalledTimes(3);
    expect(session.sending.value).toBe(false);
    session.dispose();
  });

  it("refreshes final history when the child finishes before reconnection", async () => {
    const session = createChatSession({ contextUsage: false });
    const completed = [
      subagentHistory()[0]!,
      storedMessage("assistant", [
        { type: "text", text: "finished child output" },
      ]),
    ];
    GET.mockResolvedValueOnce({ data: subagentHistory() } as never);
    GET.mockResolvedValue({ data: completed } as never);
    const fetchMock = stubFetch(
      sse([runMeta("u1"), textDelta("partial")]),
      new Response(null, { status: 404 }),
    );

    await session.attachSubagent("agent-1");

    expect(fetchMock).toHaveBeenCalledTimes(2);
    expect(session.messages.value[1]?.parts).toEqual([
      { kind: "text", text: "finished child output" },
    ]);
    expect(session.sending.value).toBe(false);
    session.dispose();
  });

  it("usage 只进实例本地快照（播种自历史，usage 事件覆盖）", async () => {
    const session = createChatSession({ contextUsage: false });
    clearContextUsage();
    GET.mockResolvedValue({
      data: [
        ...subagentHistory(),
        storedMessage("assistant", '[{"Text":"答"}]', { input_tokens: 77 }),
      ],
      error: undefined,
    } as never);
    stubFetch(new Response("gone", { status: 404 }));

    await session.attachSubagent("agent-1");

    expect(session.usage.inputTokens.value).toBe(77);
    // 全局上下文环不被子会话播种污染
    const { inputTokens } = useContextUsage();
    expect(inputTokens.value).toBeNull();
  });

  it("dispose 中断在飞订阅：不再收事件、不再收尾刷新", async () => {
    const session = createChatSession({ contextUsage: false });
    // 可暂停的 SSE
    const encoder = new TextEncoder();
    let streamController!: ReadableStreamDefaultController<Uint8Array>;
    const stream = new ReadableStream<Uint8Array>({
      start(c) {
        streamController = c;
      },
    });
    const fetchMock = vi.fn().mockResolvedValueOnce(
      new Response(stream, {
        status: 200,
        headers: { "content-type": "text/event-stream" },
      }),
    );
    vi.stubGlobal("fetch", fetchMock);
    GET.mockResolvedValue({
      data: subagentHistory(),
      error: undefined,
    } as never);

    const attachPromise = session.attachSubagent("agent-1");
    streamController.enqueue(
      encoder.encode(`data: ${JSON.stringify(runMeta("u1"))}\n\n`),
    );
    await vi.waitFor(() =>
      expect(
        session.messages.value.some((m) => RE_reattachedId.test(m.id)),
      ).toBe(true),
    );

    // reader 被主动取消，dispose 仅退订，不请求后台 cancel。
    session.dispose();
    const callsAfterDispose = GET.mock.calls.length;
    expect(fetchMock.mock.calls[0]?.[1].signal.aborted).toBe(true);
    await attachPromise;

    // 迟到事件不进视图；dispose 后没有额外的收尾刷新
    expect(GET.mock.calls.length).toBe(callsAfterDispose);
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(
      session.messages.value.some((m) =>
        m.parts.some((p) => p.kind === "text" && p.text.includes("迟到事件")),
      ),
    ).toBe(false);
    expect(session.sending.value).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// pending_recovery：send 409 的引导提示
// ---------------------------------------------------------------------------

describe("send 失败文案与 pending_recovery", () => {
  function stubFetch(...responses: Response[]) {
    const fetchMock = vi.fn();
    for (const r of responses) fetchMock.mockResolvedValueOnce(r);
    vi.stubGlobal("fetch", fetchMock);
    return fetchMock;
  }

  it("sendFailureText：code=pending_recovery 附引导文案与后端错误", async () => {
    const text = await sendFailureText(
      new Response(
        JSON.stringify({
          code: "pending_recovery",
          error: "存在未结算的子代理任务",
        }),
        { status: 409 },
      ),
    );
    expect(text).toContain("存在未完成的子代理任务");
    expect(text).toContain("「恢复此轮」");
    expect(text).toContain("存在未结算的子代理任务");
  });

  it("sendFailureText：非 JSON / 其他错误体走通用文案", async () => {
    expect(await sendFailureText(new Response("boom", { status: 500 }))).toBe(
      "Failed to send message.",
    );
    expect(
      await sendFailureText(
        new Response(JSON.stringify({ code: "conflict", error: "x" }), {
          status: 409,
        }),
      ),
    ).toBe("Failed to send message.");
  });

  it("sendMessage 收到 pending_recovery 409 时把提示渲染进消息流", async () => {
    stubFetch(
      new Response(
        JSON.stringify({
          code: "pending_recovery",
          error: "存在未结算的子代理任务",
        }),
        { status: 409 },
      ),
    );
    await sendMessage(nextConvId(), "hi");

    const text = messages.value[1]?.parts[0];
    expect(text).toMatchObject({ kind: "text" });
    expect((text as { text: string }).text).toContain("存在未完成的子代理任务");
    expect((text as { text: string }).text).toContain("「恢复此轮」");
    expect(sending.value).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// 后台任务通知：stored 行 → notification 角色 + 文本块解析
// ---------------------------------------------------------------------------

describe("后台任务通知", () => {
  it("notification user 行渲染为 notification 角色，details.tasks 与文本尾部合并", async () => {
    GET.mockResolvedValue({
      data: [
        storedMessage(
          "user",
          String.raw`[{"Text":"The following background command(s) started earlier have finished — process their results and continue:\n\n<task-notification>\ntask_id: t-1\ncommand: cargo build\nstatus: completed\nexit_code: 0\noutput_file: /var/lib/oct/exec/c1/t-1.log\noutput (tail):\n[earlier output omitted]\n   Compiling oct\n</task-notification>"}]`,
          {
            details: {
              extra: {
                kind: "background_task_notification",
                tasks: [
                  {
                    task_id: "t-1",
                    command: "cargo build",
                    status: "completed",
                    exit_code: 0,
                    error: null,
                    output_path: "/var/lib/oct/exec/c1/t-1.log",
                    created_at: "2026-09-24T00:00:00",
                    finished_at: "2026-09-24T00:01:00",
                  },
                ],
              },
            },
          },
        ),
      ],
    } as never);

    await fetchMessages(nextConvId());

    expect(messages.value).toHaveLength(1);
    const msg = messages.value[0]!;
    expect(msg.role).toBe("notification");
    const part = msg.parts[0] as {
      kind: string;
      tasks: readonly {
        taskId: string;
        command: string;
        status: string;
        exitCode: number | null;
        tail: string | null;
      }[];
    };
    expect(part.kind).toBe("task_notification");
    expect(part.tasks).toHaveLength(1);
    expect(part.tasks[0]).toMatchObject({
      taskId: "t-1",
      command: "cargo build",
      status: "completed",
      exitCode: 0,
    });
    // 尾部从 <task-notification> 文本块解析（含省略前缀）
    expect(part.tasks[0]!.tail).toBe(
      "[earlier output omitted]\n   Compiling oct",
    );
  });

  it("details.tasks 缺失时回退纯文本解析（多块 + lost 注记 + 非数字退出码）", async () => {
    GET.mockResolvedValue({
      data: [
        storedMessage(
          "user",
          String.raw`[{"Text":"<task-notification>\ntask_id: t-lost\ncommand: npm watch\nstatus: lost\nexit_code: unknown (killed by a signal)\nnote: interrupted by a backend restart — the command was terminated; the output below is partial.\noutput_file: /p/t-lost.log\noutput (tail):\npartial output\n</task-notification>\n\n<task-notification>\ntask_id: t-ok\ncommand: echo hi\nstatus: completed\nexit_code: 0\noutput_file: /p/t-ok.log\noutput (tail):\nhi\n</task-notification>"}]`,
          {
            details: {
              extra: { kind: "background_task_notification" },
            },
          },
        ),
      ],
    } as never);

    await fetchMessages(nextConvId());

    const part = messages.value[0]?.parts[0] as {
      tasks: readonly {
        taskId: string;
        exitCode: number | null;
        tail: string | null;
      }[];
    };
    expect(part.tasks.map((t) => t.taskId)).toEqual(["t-lost", "t-ok"]);
    expect(part.tasks[0]).toMatchObject({
      exitCode: null,
      tail: "partial output",
    });
    expect(part.tasks[1]).toMatchObject({ exitCode: 0, tail: "hi" });
  });

  it("末条是通知行时视为可恢复轮尾（唤醒 run 启动失败的续跑入口）", async () => {
    GET.mockResolvedValue({
      data: [
        storedMessage(
          "user",
          String.raw`[{"Text":"<task-notification>\ntask_id: t-1\ncommand: x\nstatus: completed\nexit_code: 0\noutput_file: /p\noutput (tail):\n\n</task-notification>"}]`,
          { details: { extra: { kind: "background_task_notification" } } },
        ),
      ],
    } as never);
    await fetchMessages(nextConvId());
    expect(resumable.value).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// 审批：approval_requested/resolved 事件、pending 快照恢复、
// decide 端点
// ---------------------------------------------------------------------------

describe("审批", () => {
  function stubFetch(...responses: Response[]) {
    const fetchMock = vi.fn();
    for (const r of responses) fetchMock.mockResolvedValueOnce(r);
    vi.stubGlobal("fetch", fetchMock);
    return fetchMock;
  }

  const approvalRequested = (
    approvalId: string,
    extra: Record<string, unknown> = {},
  ) => ({
    type: "approval_requested",
    data: {
      approval_id: approvalId,
      call_id: `call-${approvalId}`,
      tool: "write_file",
      kind: "write",
      title: "/tmp/outside.txt",
      path: "/tmp/outside.txt",
      command: null,
      outside: true,
      rule_suggestion: "/tmp/**",
      ...extra,
    },
  });

  const approvalResolved = (
    approvalId: string,
    outcome: string = "approved",
  ) => ({
    type: "approval_resolved",
    data: { approval_id: approvalId, outcome, rule_id: null },
  });

  function pendingOf(conv: string): number {
    return pendingApprovals.value.get(conv)?.length ?? 0;
  }

  beforeEach(() => {
    POST.mockReset();
  });

  it("approval_requested 事件 upsert 进 pending 分桶；重复到达原地覆盖", async () => {
    const conv = nextConvId();
    stubFetch(
      sse([
        approvalRequested("a1", { agent_id: "worker-a" }),
        approvalRequested("a1", { agent_id: "worker-a" }), // 重放：同 id 覆盖
        approvalRequested("a2", {
          tool: "execute_command",
          kind: "command",
          title: "cargo build",
          path: null,
          command: "cargo build",
          outside: null,
          rule_suggestion: "cargo build",
        }),
        textDelta("ok"),
        finishEvent,
      ]),
    );

    await sendMessage(conv, "hi");

    const list = pendingApprovals.value.get(conv) ?? [];
    expect(list).toHaveLength(2);
    expect(list[0]).toMatchObject({
      approvalId: "a1",
      agentId: "worker-a",
      callId: "call-a1",
      tool: "write_file",
      kind: "write",
      outside: true,
      ruleSuggestion: "/tmp/**",
    });
    expect(list[1]).toMatchObject({
      approvalId: "a2",
      kind: "command",
      command: "cargo build",
      outside: false, // null 归一为 false
    });
    // 审批事件不产生消息内容
    expect(
      messages.value.some((m) =>
        m.parts.some(
          (p) => p.kind === "text" && p.text.includes("/tmp/outside.txt"),
        ),
      ),
    ).toBe(false);
  });

  it("approval_resolved 事件移除 pending（requested→resolved 重放时序后干净）", async () => {
    const conv = nextConvId();
    stubFetch(
      sse([
        approvalRequested("a1"),
        approvalRequested("a2"),
        approvalResolved("a1", "denied"),
        approvalResolved("a2", "expired"),
        approvalResolved("a-unknown", "cancelled"), // 未命中：静默幂等
        textDelta("ok"),
        finishEvent,
      ]),
    );

    await sendMessage(conv, "hi");

    expect(pendingOf(conv)).toBe(0);
  });

  it("会话分桶隔离：另一会话的 pending 不串桶；clearMessages 清空", async () => {
    const convA = nextConvId();
    stubFetch(sse([approvalRequested("a1"), finishEvent]));
    await sendMessage(convA, "hi");

    const convB = nextConvId();
    stubFetch(sse([approvalRequested("b1"), finishEvent]));
    await sendMessage(convB, "hi");

    expect(pendingOf(convA)).toBe(1);
    expect(pendingOf(convB)).toBe(1);

    clearMessages();
    expect(pendingOf(convA)).toBe(0);
    expect(pendingOf(convB)).toBe(0);
  });

  it("decideApproval：POST 端点与 body 正确；成功/404 移除本地，其他错误保留", async () => {
    const conv = nextConvId();
    stubFetch(
      sse([
        approvalRequested("a1"),
        approvalRequested("a2"),
        approvalRequested("a3"),
        finishEvent,
      ]),
    );
    await sendMessage(conv, "hi");
    expect(pendingOf(conv)).toBe(3);

    // 成功：本地即清（approval_resolved 事件再清一次是幂等空操作）
    POST.mockResolvedValueOnce({
      data: { resolved: true },
      error: undefined,
      response: new Response(),
    } as never);
    await decideApproval(conv, "a1", "allow");
    expect(POST).toHaveBeenCalledWith(
      "/api/conversations/{id}/approvals/{approval_id}",
      {
        params: { path: { id: conv, approval_id: "a1" } },
        body: { decision: "allow", pattern: null },
      },
    );
    expect(pendingOf(conv)).toBe(2);

    // 404（已决/未知 id）：静默清掉本地条目
    POST.mockResolvedValueOnce({
      data: undefined,
      error: { message: "unknown" },
      response: new Response(null, { status: 404 }),
    } as never);
    await decideApproval(conv, "a2", "deny");
    expect(pendingOf(conv)).toBe(1);

    // 其他错误（如 500）：保留卡片供重试；pattern 随 allow_always 提交
    POST.mockResolvedValueOnce({
      data: undefined,
      error: { message: "boom" },
      response: new Response(null, { status: 500 }),
    } as never);
    await decideApproval(conv, "a3", "allow_always", "/custom/**");
    expect(POST).toHaveBeenLastCalledWith(
      "/api/conversations/{id}/approvals/{approval_id}",
      {
        params: { path: { id: conv, approval_id: "a3" } },
        body: { decision: "allow_always", pattern: "/custom/**" },
      },
    );
    expect(pendingOf(conv)).toBe(1);
  });

  it("attachRun：run 运行中先拉 pending 快照，以 GET 为准整表替换", async () => {
    const conv = nextConvId();
    // 预置一条陈旧本地条目（后端早已决，但事件未及到达的模拟）
    stubFetch(sse([approvalRequested("stale"), finishEvent]));
    await sendMessage(conv, "hi");
    expect(pendingOf(conv)).toBe(1);

    // 重连：/run 探测 running → GET approvals 返回空 → 重放流无审批事件
    GET.mockClear(); // 排除首次 send 的终态历史刷新。
    GET.mockResolvedValueOnce({ data: { running: true } } as never);
    GET.mockResolvedValueOnce({ data: [] } as never); // approvals 快照
    GET.mockResolvedValueOnce({ data: undefined, error: {} } as never); // 终态刷新失败：保留视图
    stubFetch(sse([textDelta("续"), finishEvent]));

    await attachRun(conv);

    // GET 轨迹：/run → approvals →（事件流）→ messages
    expect(GET.mock.calls[0]?.[0]).toBe("/api/conversations/{id}/run");
    expect(GET.mock.calls[1]?.[0]).toBe("/api/conversations/{id}/approvals");
    expect(GET.mock.calls[1]?.[1]).toEqual({
      params: { path: { id: conv } },
    });
    // 陈旧条目被快照清掉（以 GET 为准）
    expect(pendingOf(conv)).toBe(0);
  });

  it("切回空闲会话时对账已错过的审批决定", async () => {
    const session = createChatSession({ contextUsage: false });
    let controller!: ReadableStreamDefaultController<Uint8Array>;
    const response = new Response(
      new ReadableStream<Uint8Array>({
        start(value) {
          controller = value;
        },
      }),
    );
    const fetchMock = stubFetch(response);
    GET.mockResolvedValue({ data: [] } as never);
    try {
      const run = session.sendMessage("A", "write");
      controller.enqueue(
        new TextEncoder().encode(
          `data: ${JSON.stringify(approvalRequested("resolved-while-away"))}\n\n`,
        ),
      );
      await vi.waitFor(() =>
        expect(session.pendingApprovals.value.get("A")?.length).toBe(1),
      );
      await session.fetchMessages("B");
      expect(fetchMock.mock.calls[0]?.[1].signal.aborted).toBe(true);
      await run;
      await session.fetchMessages("A");
      GET.mockClear();
      GET.mockResolvedValueOnce({ data: { running: false } } as never);
      GET.mockResolvedValueOnce({ data: [] } as never);
      GET.mockResolvedValueOnce({ data: [] } as never);
      await session.attachRun("A");
      expect(session.pendingApprovals.value.get("A")).toEqual([]);
      expect(GET.mock.calls.map((call) => call[0])).toEqual([
        "/api/conversations/{id}/run",
        "/api/conversations/{id}/approvals",
        "/api/conversations/{id}/messages",
      ]);
      expect(fetchMock).toHaveBeenCalledTimes(1);
    } finally {
      session.dispose();
    }
  });

  it("permission_denied 工具卡：live 事件与历史行两路都标记 denied", async () => {
    // live：tool_result 的 details 顶层 kind=permission_denied
    stubFetch(
      sse([
        toolCallStart("c1", "write_file", "/etc/hosts", "{}"),
        toolResult("c1", "工具调用被拒绝：用户拒绝了本次写入。", true, {
          kind: "permission_denied",
        }),
        finishEvent,
      ]),
    );
    await sendMessage(nextConvId(), "hi");
    expect(toolCards()[0]!.denied).toBe(true);
    expect(toolCards()[0]!.isError).toBe(true);

    // 历史：DTO details 的已知 kind（typed 字段，不进 extra）
    GET.mockResolvedValue({
      data: [
        storedMessage(
          "assistant",
          '[{"ToolCall":{"id":"c2","name":"write_file","arguments":"{}"}}]',
        ),
        storedMessage(
          "tool",
          '[{"ToolResult":{"call_id":"c2","content":"被拒绝","is_error":true}}]',
          { details: { kind: "permission_denied" } },
        ),
      ],
    } as never);
    await fetchMessages(nextConvId());
    expect(toolCards()[0]!.denied).toBe(true);
  });
});

describe("history request ownership", () => {
  it("a late response from the previous conversation cannot replace the current history", async () => {
    const session = createChatSession({ contextUsage: false });
    let finishOld!: (value: unknown) => void;
    GET.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          finishOld = resolve;
        }) as never,
    );
    GET.mockResolvedValueOnce({
      data: [storedMessage("user", "new conversation")],
    } as never);
    const old = session.fetchMessages("old-history");
    await session.fetchMessages("new-history");
    finishOld({ data: [storedMessage("user", "stale conversation")] });
    await old;
    expect(JSON.stringify(session.messages.value)).toContain(
      "new conversation",
    );
    expect(JSON.stringify(session.messages.value)).not.toContain(
      "stale conversation",
    );
    session.dispose();
  });

  it("disposing a subagent while history is loading prevents a late update", async () => {
    const session = createChatSession({ contextUsage: false });
    let finish!: (value: unknown) => void;
    GET.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          finish = resolve;
        }) as never,
    );
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(new Response(null, { status: 404 })),
    );
    const loading = session.attachSubagent("disposed-agent");
    session.dispose();
    finish({ data: [storedMessage("user", "late child history")] });
    await loading;
    expect(session.messages.value).toEqual([]);
  });
});

describe("stream failure recovery", () => {
  it("reattaches after incomplete EOF without repeating the side-effecting POST", async () => {
    const session = createChatSession({ contextUsage: false });
    const request = vi.fn().mockResolvedValue(sse([textDelta("partial")]));
    vi.stubGlobal("fetch", request);
    GET.mockImplementation(async (path) => {
      if (path === "/api/conversations/{id}/run") {
        return { data: { running: false, resumable: true } } as never;
      }
      return { data: [storedMessage("user", "original request")] } as never;
    });
    await session.sendMessage("interrupted", "original request");
    expect(request).toHaveBeenCalledTimes(1);
    expect(
      GET.mock.calls.some((call) => call[0] === "/api/conversations/{id}/run"),
    ).toBe(true);
    expect(session.resumable.value).toBe(true);
    session.dispose();
  });

  it("uses server recovery eligibility after a failed tool-tail turn", async () => {
    const session = createChatSession({ contextUsage: false });
    vi.stubGlobal(
      "fetch",
      vi
        .fn()
        .mockResolvedValue(
          sse([
            toolCallStart("call", "list_dir", "list", "{}"),
            toolResult("call", "done"),
            errorEvent("provider disconnected"),
          ]),
        ),
    );
    GET.mockResolvedValue({
      data: { running: false, resumable: true },
    } as never);
    await session.sendMessage("failed-tool-tail", "list files");
    expect(session.resumable.value).toBe(true);
    session.dispose();
  });

  it.each([true, false])(
    "clears recovery eligibility after a successful resume (history available: %s)",
    async (historyAvailable) => {
      const session = createChatSession({ contextUsage: false });
      const conv = nextConvId();
      const original = storedMessage("user", "original request");
      const completed = storedMessage("assistant", "recovered reply");
      GET.mockResolvedValue({ data: [original] } as never);
      await session.fetchMessages(conv);
      GET.mockResolvedValueOnce({
        data: { running: false, resumable: true },
      } as never);
      await session.attachRun(conv);
      expect(session.resumable.value).toBe(true);

      GET.mockResolvedValueOnce({ data: [original] } as never);
      GET.mockResolvedValueOnce(
        historyAvailable
          ? ({ data: [original, completed] } as never)
          : ({ error: { message: "history unavailable" } } as never),
      );
      vi.stubGlobal(
        "fetch",
        vi
          .fn()
          .mockResolvedValue(sse([textDelta("recovered reply"), finishEvent])),
      );
      await session.resumeTurn(conv);

      expect(JSON.stringify(session.messages.value)).toContain(
        "recovered reply",
      );
      expect(session.resumable.value).toBe(false);
      session.dispose();
    },
  );
});

it("optimistic messages distinguish image and text attachments", async () => {
  const session = createChatSession({ contextUsage: false });
  let resolveResponse!: (response: Response) => void;
  const response = new Promise<Response>((resolve) => {
    resolveResponse = resolve;
  });
  const request = vi.fn().mockReturnValue(response);
  vi.stubGlobal("fetch", request);
  const sending = session.sendMessage("attachments", "Read these", null, null, [
    { attachmentId: "image-1", filename: "shot.png", mimeType: "image/png" },
    { attachmentId: "text-1", filename: "notes.txt", mimeType: "text/plain" },
  ]);
  try {
    expect(session.messages.value[0]?.parts).toEqual([
      { kind: "text", text: "Read these" },
      { kind: "image", attachmentId: "image-1", filename: "shot.png" },
      { kind: "file", attachmentId: "text-1", filename: "notes.txt" },
    ]);
    const body = JSON.parse(request.mock.calls[0]![1].body);
    expect(body.attachment_ids).toEqual(["image-1", "text-1"]);
  } finally {
    resolveResponse(sse([finishEvent]));
    await sending;
    session.dispose();
  }
});

describe("send acceptance", () => {
  beforeEach(() => {
    GET.mockResolvedValue({ data: [] } as never);
  });

  it("confirms once when the response opens, before the stream finishes", async () => {
    const session = createChatSession({ contextUsage: false });
    const accepted = vi.fn();
    let stream!: ReadableStreamDefaultController<Uint8Array>;
    let resolveResponse!: (response: Response) => void;
    const response = new Promise<Response>((resolve) => {
      resolveResponse = resolve;
    });
    vi.stubGlobal("fetch", vi.fn().mockReturnValue(response));
    const sending = session.sendMessage(
      "acceptance",
      "hello",
      null,
      null,
      [],
      accepted,
    );
    try {
      expect(accepted).not.toHaveBeenCalled();
      resolveResponse(
        new Response(
          new ReadableStream<Uint8Array>({
            start(controller) {
              stream = controller;
            },
          }),
          { headers: { "Content-Type": "text/event-stream" } },
        ),
      );
      await vi.waitFor(() => expect(accepted).toHaveBeenCalledTimes(1));
      expect(session.sending.value).toBe(true);
    } finally {
      stream.enqueue(
        new TextEncoder().encode(`data: ${JSON.stringify(finishEvent)}\n\n`),
      );
      stream.close();
      await sending;
      session.dispose();
    }
    expect(accepted).toHaveBeenCalledTimes(1);
  });

  it.each([400, 409, 500])(
    "does not confirm a rejected send (HTTP %s)",
    async (status) => {
      const session = createChatSession({ contextUsage: false });
      const accepted = vi.fn();
      vi.stubGlobal(
        "fetch",
        vi.fn().mockResolvedValue(new Response("rejected", { status })),
      );
      await session.sendMessage(
        "rejected-acceptance",
        "keep draft",
        null,
        null,
        [],
        accepted,
      );
      expect(accepted).not.toHaveBeenCalled();
      session.dispose();
    },
  );

  it("does not confirm an old response after the user switches conversation", async () => {
    const session = createChatSession({ contextUsage: false });
    const accepted = vi.fn();
    let resolveResponse!: (response: Response) => void;
    vi.stubGlobal(
      "fetch",
      vi.fn().mockReturnValue(
        new Promise<Response>((resolve) => {
          resolveResponse = resolve;
        }),
      ),
    );
    const sending = session.sendMessage(
      "old-acceptance",
      "old draft",
      null,
      null,
      [],
      accepted,
    );
    await session.fetchMessages("new-acceptance");
    resolveResponse(sse([finishEvent]));
    await sending;
    expect(accepted).not.toHaveBeenCalled();
    session.dispose();
  });

  it.each([409, 500, 502])(
    "confirms committed attachments after HTTP %s and exposes resume",
    async (status) => {
      const session = createChatSession({ contextUsage: false });
      let clientRequestId: string | undefined;
      const persisted = storedMessage("user", "read notes", {
        id: "committed-attachment-user",
        details: {
          extra: {
            attachments: [
              { id: "text-1", filename: "notes.txt", mime_type: "text/plain" },
            ],
          },
        },
      });
      GET.mockImplementation(async (path) => {
        if (path === "/api/conversations/{id}/run")
          return { data: { running: false, resumable: true } } as never;
        if (path === "/api/conversations/{id}/messages") {
          persisted.details!.extra!.client_request_id = clientRequestId;
          return {
            data: [persisted],
            response: new Response(null, {
              headers: { "X-Context-Revision": "1" },
            }),
          } as never;
        }
        return { data: [] } as never;
      });
      const request = vi.fn<typeof fetch>(async (_path, init) => {
        clientRequestId = JSON.parse(init!.body as string).client_request_id;
        return new Response(
          JSON.stringify({ code: "internal", error: "run preparation failed" }),
          { status },
        );
      });
      vi.stubGlobal("fetch", request);
      const accepted = vi.fn();
      try {
        await session.sendMessage(
          "committed-error",
          "read notes",
          null,
          null,
          [
            {
              attachmentId: "text-1",
              filename: "notes.txt",
              mimeType: "text/plain",
            },
          ],
          accepted,
        );
        expect(accepted).toHaveBeenCalledTimes(1);
        expect(request).toHaveBeenCalledTimes(1);
        expect(session.messages.value.map((message) => message.id)).toEqual([
          persisted.id,
        ]);
        expect(session.messages.value[0]?.parts).toContainEqual({
          kind: "file",
          attachmentId: "text-1",
          filename: "notes.txt",
        });
        expect(session.contextRevision.value).toBe(1);
        expect(session.resumable.value).toBe(true);
        expect(session.sending.value).toBe(false);
      } finally {
        session.dispose();
      }
    },
  );
});

describe("failed sends with versioned history", () => {
  it.each(["idle", "running", "resumable"] as const)(
    "an unrelated %s history change cannot erase an uncertain send",
    async (status) => {
      const session = createChatSession({ contextUsage: false });
      const before = [storedMessage("assistant", "previous", { id: "before" })];
      const unrelated = [
        ...before,
        // Identical text from another tab still is not this request's receipt.
        storedMessage("user", "keep this draft", {
          id: "other-user",
          details: { extra: { client_request_id: "other-request" } },
        }),
        storedMessage("assistant", "another tab's answer", {
          id: "other-answer",
        }),
      ];
      let changed = false;
      GET.mockImplementation(async (path: string) => {
        if (path === "/api/conversations/{id}/run")
          return {
            data: {
              running: status === "running",
              resumable: status === "resumable",
            },
          } as never;
        return {
          data: changed ? unrelated : before,
          response: new Response(null, {
            headers: { "X-Context-Revision": changed ? "3" : "1" },
          }),
        } as never;
      });
      await session.fetchMessages("uncertain-send");
      const request = vi.fn().mockImplementation(async () => {
        changed = true;
        throw new TypeError("connection reset before delivery");
      });
      vi.stubGlobal("fetch", request);
      const accepted = vi.fn();
      try {
        await session.sendMessage(
          "uncertain-send",
          "keep this draft",
          null,
          null,
          [],
          accepted,
        );
        expect(accepted).not.toHaveBeenCalled();
        expect(session.messages.value.map((message) => message.id)).toContain(
          "before",
        );
        expect(session.messages.value[1]?.id.startsWith("pending-")).toBe(true);
        expect(session.messages.value[1]?.parts).toEqual([
          { kind: "text", text: "keep this draft" },
        ]);
        expect(JSON.stringify(session.messages.value[2]?.parts)).toContain(
          "Failed to send message.",
        );
        expect(request).toHaveBeenCalledTimes(1);
        expect(session.sending.value).toBe(false);
      } finally {
        session.dispose();
      }
    },
  );

  it.each(["running", "completed"] as const)(
    "a lost POST response reconciles an accepted %s send without repeating it",
    async (status) => {
      const session = createChatSession({ contextUsage: false });
      const before = [storedMessage("assistant", "previous", { id: "before" })];
      const persisted = [
        ...before,
        storedMessage("user", "accepted request", { id: "accepted-user" }),
        storedMessage("assistant", "accepted answer", {
          id: "accepted-answer",
        }),
      ];
      let committed = false;
      let clientRequestId: string | undefined;
      GET.mockImplementation(async (path: string) => {
        if (path === "/api/conversations/{id}/run") {
          return {
            data: { running: status === "running", resumable: false },
          } as never;
        }
        if (path === "/api/conversations/{id}/approvals")
          return { data: [] } as never;
        if (committed)
          persisted[1]!.details = {
            extra: { client_request_id: clientRequestId },
          };
        return {
          data: committed ? persisted : before,
          response: new Response(null, {
            headers: { "X-Context-Revision": committed ? "3" : "1" },
          }),
        } as never;
      });
      await session.fetchMessages("lost-response");
      const request = vi
        .fn()
        .mockImplementation(async (path: string, init?: RequestInit) => {
          if (path.endsWith("/messages")) {
            clientRequestId = JSON.parse(
              init?.body as string,
            ).client_request_id;
            committed = true;
            throw new TypeError("response headers lost");
          }
          return sse([
            runMeta("accepted-user"),
            textDelta("accepted answer"),
            finishEvent,
          ]);
        });
      vi.stubGlobal("fetch", request);
      const accepted = vi.fn();
      await session.sendMessage(
        "lost-response",
        "accepted request",
        null,
        null,
        [],
        accepted,
      );
      expect(accepted).toHaveBeenCalledTimes(1);
      expect(clientRequestId).toEqual(expect.any(String));
      expect(
        request.mock.calls.filter(([path]) => path.endsWith("/messages")),
      ).toHaveLength(1);
      expect(
        request.mock.calls.filter(([path]) => path.endsWith("/events")),
      ).toHaveLength(status === "running" ? 1 : 0);
      expect(session.messages.value.map((message) => message.id)).toEqual(
        persisted.map((message) => message.id),
      );
      expect(session.contextRevision.value).toBe(3);
      expect(session.sending.value).toBe(false);
      session.dispose();
    },
  );

  it("an unreachable backend preserves the uncertain draft and failure for recovery", async () => {
    const session = createChatSession({ contextUsage: false });
    GET.mockResolvedValueOnce({
      data: [],
      response: new Response(null, { headers: { "X-Context-Revision": "1" } }),
    } as never);
    await session.fetchMessages("unreachable-send");
    GET.mockRejectedValue(new TypeError("offline"));
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new TypeError("offline")));
    await session.sendMessage("unreachable-send", "keep this draft");
    expect(session.messages.value[0]?.parts).toEqual([
      { kind: "text", text: "keep this draft" },
    ]);
    expect(JSON.stringify(session.messages.value[1]?.parts)).toContain(
      "Failed to send message.",
    );
    expect(session.sending.value).toBe(false);
    session.dispose();
  });

  it.each(["http", "pending_recovery", "network"] as const)(
    "preserves the rejected prompt and failure explanation (%s)",
    async (failure) => {
      const session = createChatSession({ contextUsage: false });
      const history = [storedMessage("assistant", "previous reply")];
      GET.mockResolvedValue({
        data: history,
        response: new Response(null, {
          headers: { "X-Context-Revision": "1" },
        }),
      } as never);
      await session.fetchMessages("rejected-send");
      const request = vi.fn();
      if (failure === "network") {
        request.mockRejectedValue(new TypeError("network unavailable"));
      } else {
        request.mockResolvedValue(
          failure === "pending_recovery"
            ? new Response(
                JSON.stringify({
                  code: "pending_recovery",
                  error: "存在未结算的子代理任务",
                }),
                { status: 409 },
              )
            : new Response("boom", { status: 500 }),
        );
      }
      vi.stubGlobal("fetch", request);

      await session.sendMessage("rejected-send", "rejected prompt");

      expect(session.messages.value).toHaveLength(3);
      expect(session.messages.value[0]?.id).toBe(history[0]?.id);
      expect(session.messages.value[1]?.parts).toEqual([
        { kind: "text", text: "rejected prompt" },
      ]);
      expect(JSON.stringify(session.messages.value[2]?.parts)).toContain(
        "Failed to send message.",
      );
      if (failure === "pending_recovery") {
        expect(JSON.stringify(session.messages.value[2]?.parts)).toContain(
          "恢复此轮",
        );
      }
      expect(session.messages.value[2]?.isStreaming).toBe(false);
      expect(session.sending.value).toBe(false);
      expect(session.contextRevision.value).toBeNull();
      session.dispose();
    },
  );

  it("keeps stale-send retries revision-checked until history is refreshed", async () => {
    const session = createChatSession({ contextUsage: false });
    GET.mockResolvedValue({
      data: [storedMessage("assistant", "old branch")],
      response: new Response(null, { headers: { "X-Context-Revision": "4" } }),
    } as never);
    await session.fetchMessages("stale-send");
    const request = vi.fn<typeof fetch>(
      async () =>
        new Response(JSON.stringify({ code: "stale_revision" }), {
          status: 409,
        }),
    );
    vi.stubGlobal("fetch", request);

    await session.sendMessage("stale-send", "first attempt");
    expect(JSON.stringify(session.messages.value)).toContain("会话历史已变化");
    expect(session.sending.value).toBe(false);
    await session.sendMessage("stale-send", "retry");

    expect(request).toHaveBeenCalledTimes(2);
    for (const call of request.mock.calls) {
      const init = call[1] as RequestInit;
      expect(JSON.parse(init.body as string)).toMatchObject({
        expected_revision: 4,
      });
    }
    // Receipt probes without this request's id must not replace the stale
    // snapshot or authorize history actions against an unrelated revision.
    expect(session.contextRevision.value).toBeNull();

    GET.mockResolvedValue({
      data: [storedMessage("assistant", "reverted branch")],
      response: new Response(null, { headers: { "X-Context-Revision": "5" } }),
    } as never);
    await session.refreshMessages("stale-send");
    await session.sendMessage("stale-send", "refreshed attempt");
    const init = request.mock.calls[2]?.[1] as RequestInit;
    expect(JSON.parse(init.body as string)).toMatchObject({
      expected_revision: 5,
    });
    session.dispose();
  });

  it.each(["original", "other"])(
    "a late send rejection preserves the newer snapshot revision (%s)",
    async (current) => {
      const session = createChatSession({ contextUsage: false });
      GET.mockResolvedValueOnce({
        data: [],
        response: new Response(null, {
          headers: { "X-Context-Revision": "4" },
        }),
      } as never);
      await session.fetchMessages("original");
      let rejectSend!: (response: Response) => void;
      const request = vi.fn<typeof fetch>(
        () =>
          new Promise<Response>((resolve) => {
            rejectSend = resolve;
          }),
      );
      vi.stubGlobal("fetch", request);
      const sending = session.sendMessage("original", "rejected prompt");
      GET.mockResolvedValueOnce({
        data: [storedMessage("assistant", "current history")],
        response: new Response(null, {
          headers: { "X-Context-Revision": "8" },
        }),
      } as never);
      await session.refreshMessages(current);
      rejectSend(
        new Response(JSON.stringify({ code: "stale_revision" }), {
          status: 409,
        }),
      );
      await sending;

      expect(session.contextRevision.value).toBe(8);
      expect(session.messages.value[0]?.parts).toEqual([
        { kind: "text", text: "current history" },
      ]);
      request.mockResolvedValueOnce(new Response("rejected", { status: 409 }));
      await session.sendMessage(current, "current attempt");
      const init = request.mock.calls[1]?.[1] as RequestInit;
      expect(JSON.parse(init.body as string)).toMatchObject({
        expected_revision: 8,
      });
      session.dispose();
    },
  );

  it.each(["", "partial reply"])(
    "preserves SSE errors and partial output (%s)",
    async (partial) => {
      const session = createChatSession({ contextUsage: false });
      const history = [storedMessage("assistant", "previous reply")];
      const persisted = [
        ...history,
        storedMessage("user", "accepted prompt"),
        ...(partial ? [storedMessage("assistant", partial)] : []),
      ];
      let sent = false;
      GET.mockImplementation(async (path) => {
        if (path === "/api/conversations/{id}/run") {
          return { data: { running: false, resumable: true } } as never;
        }
        return {
          data: sent ? persisted : history,
          response: new Response(null, {
            headers: { "X-Context-Revision": sent ? "3" : "1" },
          }),
        } as never;
      });
      await session.fetchMessages("failed-stream");
      sent = true;
      vi.stubGlobal(
        "fetch",
        vi
          .fn()
          .mockResolvedValue(
            sse([
              ...(partial ? [textDelta(partial)] : []),
              errorEvent("provider disconnected"),
            ]),
          ),
      );

      await session.sendMessage("failed-stream", "accepted prompt");

      expect(session.messages.value).toHaveLength(3);
      expect(session.messages.value[1]?.parts).toEqual([
        { kind: "text", text: "accepted prompt" },
      ]);
      expect(session.messages.value[2]?.parts).toEqual([
        {
          kind: "text",
          text: `${partial ? `${partial}\n\n` : ""}Error: provider disconnected`,
        },
      ]);
      expect(session.messages.value[2]?.isStreaming).toBe(false);
      expect(session.sending.value).toBe(false);
      expect(session.resumable.value).toBe(true);
      expect(session.contextRevision.value).toBeNull();
      session.dispose();
    },
  );
});

it("fork 快照使用响应版本；流结束后动作换成真实持久化消息 id", async () => {
  const session = createChatSession({ contextUsage: false });
  GET.mockResolvedValueOnce({
    data: [
      storedMessage("user", [{ type: "text", text: "old" }], {
        id: "persisted-user",
        fork_cut: {
          kind: "message",
          message_id: "persisted-user",
          side: "before",
        },
      }),
    ],
    response: new Response(null, { headers: { "X-Context-Revision": "1" } }),
  } as never);
  await session.fetchMessages("fork-versioned");
  expect(session.contextRevision.value).toBe(1);
  expect(session.messages.value[0]?.forkCut).toMatchObject({
    message_id: "persisted-user",
  });
  GET.mockResolvedValueOnce({
    data: [
      storedMessage("assistant", [{ type: "text", text: "reply" }], {
        id: "persisted-reply",
        fork_cut: {
          kind: "message",
          message_id: "persisted-reply",
          side: "after",
        },
      }),
    ],
    response: new Response(null, { headers: { "X-Context-Revision": "3" } }),
  } as never);
  vi.stubGlobal(
    "fetch",
    vi.fn().mockResolvedValueOnce(sse([textDelta("reply"), finishEvent])),
  );
  await session.sendMessage("fork-versioned", "new question");
  expect(session.contextRevision.value).toBe(3);
  expect(session.messages.value[0]?.id).toBe("persisted-reply");
  expect(session.messages.value[0]?.forkCut).toMatchObject({
    message_id: "persisted-reply",
    side: "after",
  });
  session.dispose();
});

it("回退时阻止新发送/重连，并忽略回退前的迟到历史响应", async () => {
  const session = createChatSession();
  let resolveOld!: (value: unknown) => void;
  GET.mockImplementationOnce(
    () =>
      new Promise((resolve) => {
        resolveOld = resolve;
      }) as never,
  );
  const oldLoad = session.refreshMessages("c1");
  expect(session.beginHistoryChange()).toBe(true);
  const fetch = vi.fn();
  vi.stubGlobal("fetch", fetch);
  await session.sendMessage("c1", "blocked", "mock:model");
  await session.attachRun("c1");
  expect(fetch).not.toHaveBeenCalled();
  resolveOld({
    data: [storedMessage("assistant", [{ type: "text", text: "old suffix" }])],
    response: new Response(null, { headers: { "X-Context-Revision": "4" } }),
  });
  await oldLoad;
  expect(session.messages.value).toHaveLength(0);
  GET.mockResolvedValueOnce({
    data: [storedMessage("assistant", [{ type: "text", text: "kept prefix" }])],
    response: new Response(null, {
      headers: { "X-Context-Revision": "5", "X-Context-Tokens": "123" },
    }),
  } as never);
  GET.mockResolvedValueOnce({
    data: { running: false, resumable: false },
  } as never);
  await session.endHistoryChange("c1", true);
  expect(session.messages.value[0]?.parts).toEqual([
    { kind: "text", text: "kept prefix" },
  ]);
  expect(session.contextRevision.value).toBe(5);
  expect(session.usage.inputTokens.value).toBe(123);
  expect(session.resumable.value).toBe(false);
});

it("reattaches the running conversation selected during a history change", async () => {
  const session = createChatSession({ contextUsage: false });
  const anchor = storedMessage("user", "running request", {
    id: "running-user",
  });
  let finished = false;
  GET.mockImplementation(async (path, init) => {
    const id = (init as { params: { path: { id: string } } }).params.path.id;
    if (path === "/api/conversations/{id}/run") {
      expect(id).toBe("running");
      return { data: { running: true, resumable: false } } as never;
    }
    if (path === "/api/conversations/{id}/approvals") {
      expect(id).toBe("running");
      return { data: [] } as never;
    }
    return {
      data:
        id === "original"
          ? [storedMessage("assistant", "original history")]
          : [
              anchor,
              ...(finished ? [storedMessage("assistant", "live reply")] : []),
            ],
      response: new Response(null, { headers: { "X-Context-Revision": "5" } }),
    } as never;
  });
  const encoder = new TextEncoder();
  let controller!: ReadableStreamDefaultController<Uint8Array>;
  const stream = new ReadableStream<Uint8Array>({
    start(c) {
      controller = c;
    },
  });
  const request = vi.fn().mockResolvedValueOnce(
    new Response(stream, {
      headers: { "Content-Type": "text/event-stream" },
    }),
  );
  vi.stubGlobal("fetch", request);
  const send = (...events: unknown[]) =>
    controller.enqueue(
      encoder.encode(
        `${events.map((event) => `data: ${JSON.stringify(event)}`).join("\n\n")}\n\n`,
      ),
    );

  try {
    await session.fetchMessages("original");
    expect(session.beginHistoryChange()).toBe(true);
    await session.fetchMessages("running");
    await session.attachRun("running");
    expect(GET).toHaveBeenCalledTimes(1);
    expect(request).not.toHaveBeenCalled();

    // Completion must release the history action while the live run continues.
    await session.endHistoryChange("running", true);
    await vi.waitFor(() =>
      expect(request).toHaveBeenCalledWith(
        "/api/conversations/running/events",
        expect.objectContaining({ signal: expect.any(AbortSignal) }),
      ),
    );
    expect(session.sending.value).toBe(true);
    send(runMeta(anchor.id), textDelta("live reply"), {
      type: "approval_requested",
      data: {
        approval_id: "live-approval",
        call_id: "live-call",
        tool: "write_file",
        kind: "write",
        title: "output.txt",
      },
    });
    await vi.waitFor(() => {
      expect(session.messages.value.at(-1)?.parts).toEqual([
        { kind: "text", text: "live reply" },
      ]);
      expect(session.pendingApprovals.value.get("running")).toMatchObject([
        { approvalId: "live-approval" },
      ]);
    });
    finished = true;
    send(finishEvent);
  } finally {
    controller.close();
    await vi.waitFor(() => expect(session.sending.value).toBe(false));
    session.dispose();
  }
  expect(session.messages.value.at(-1)?.parts).toEqual([
    { kind: "text", text: "live reply" },
  ]);
});

it("发送携带展示历史的版本，防止旧窗口回退后追加旧分支输入", async () => {
  const session = createChatSession();
  GET.mockResolvedValue({
    data: [],
    response: new Response(null, { headers: { "X-Context-Revision": "8" } }),
  } as never);
  await session.refreshMessages("c1");
  const fetch = vi
    .fn()
    .mockResolvedValue(sse([{ type: "finish", data: { message: "done" } }]));
  vi.stubGlobal("fetch", fetch);
  await session.sendMessage("c1", "new", "mock:model");
  const init = fetch.mock.calls[0]?.[1] as RequestInit;
  expect(JSON.parse(init.body as string)).toMatchObject({
    content: "new",
    expected_revision: 8,
  });
});

// Drive public APIs with delayed network responses; even a fetch mock that
// ignores AbortSignal must not give an obsolete subscription ownership again.
describe("流所有权", () => {
  function deferred<T>() {
    let resolve!: (value: T) => void;
    const promise = new Promise<T>((done) => {
      resolve = done;
    });
    return { promise, resolve };
  }

  function openStream() {
    let controller!: ReadableStreamDefaultController<Uint8Array>;
    const response = new Response(
      new ReadableStream<Uint8Array>({
        start(c) {
          controller = c;
        },
      }),
    );
    return {
      response,
      send(...events: unknown[]) {
        controller.enqueue(
          new TextEncoder().encode(
            `${events.map((event) => `data: ${JSON.stringify(event)}`).join("\n\n")}\n\n`,
          ),
        );
      },
      close() {
        controller.close();
      },
    };
  }

  const usageEvent = (input: number) => ({
    type: "usage",
    data: {
      input_tokens: input,
      output_tokens: 1,
      context_window_tokens: 10000,
    },
  });

  it.each(["B", "A"])(
    "回退后的迟到刷新不能把旧恢复状态写入新视图 %s",
    async (currentId) => {
      const session = createChatSession({ contextUsage: false });
      const oldHistory = deferred<unknown>();
      GET.mockReturnValueOnce(oldHistory.promise as never);
      GET.mockImplementation(async (path, init) => {
        if (path === "/api/conversations/{id}/run")
          return { data: { running: false, resumable: true } } as never;
        const id = (init as { params: { path: { id: string } } }).params.path
          .id;
        return {
          data: [storedMessage("assistant", `${id} current`)],
          response: new Response(null, {
            headers: { "X-Context-Revision": "8", "X-Context-Tokens": "123" },
          }),
        } as never;
      });
      try {
        expect(session.beginHistoryChange()).toBe(true);
        const oldRefresh = session.endHistoryChange("A", true);
        await session.refreshMessages("B");
        if (currentId === "A") await session.refreshMessages("A");
        oldHistory.resolve({
          data: [storedMessage("assistant", "obsolete A")],
          response: new Response(null, {
            headers: { "X-Context-Revision": "7", "X-Context-Tokens": "9999" },
          }),
        });
        await oldRefresh;
        expect(session.messages.value.at(-1)?.parts).toEqual([
          { kind: "text", text: `${currentId} current` },
        ]);
        expect(session.contextRevision.value).toBe(8);
        expect(session.usage.inputTokens.value).toBe(123);
        expect(session.resumable.value).toBe(false);
        expect(
          GET.mock.calls.some(
            (call) => call[0] === "/api/conversations/{id}/run",
          ),
        ).toBe(false);
      } finally {
        session.dispose();
      }
    },
  );

  it("切到 B 后历史尚未返回就发送，不沿用 A 的消息", async () => {
    const session = createChatSession({ contextUsage: false });
    GET.mockResolvedValueOnce({
      data: [storedMessage("assistant", "A history")],
    } as never);
    await session.refreshMessages("A");
    const delayedHistory = deferred<unknown>();
    GET.mockReturnValueOnce(delayedHistory.promise as never);
    const loading = session.refreshMessages("B");
    expect(session.messages.value).toEqual([]);
    GET.mockResolvedValue({ data: [] } as never);
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(sse([textDelta("B reply"), finishEvent])),
    );
    await session.sendMessage("B", "B question");
    delayedHistory.resolve({
      data: [storedMessage("assistant", "stale B history")],
    });
    await loading;
    expect(session.messages.value.flatMap((message) => message.parts)).toEqual([
      { kind: "text", text: "B question" },
      { kind: "text", text: "B reply" },
    ]);
    session.dispose();
  });

  it("切换 A→B→A 后旧 fetch、事件和 finally 不影响新流的消息、usage、审批或 busy", async () => {
    const session = createChatSession();
    const oldResponse = deferred<Response>();
    const current = openStream();
    const fetch = vi
      .fn()
      .mockReturnValueOnce(oldResponse.promise)
      .mockResolvedValueOnce(current.response);
    vi.stubGlobal("fetch", fetch);
    GET.mockResolvedValue({ data: [] } as never);
    const titles: string[] = [];
    const unsubscribe = onTitleUpdated((_id, title) => titles.push(title));
    try {
      await session.refreshMessages("A");
      const oldRun = session.sendMessage("A", "old request");
      await session.refreshMessages("B");
      expect((fetch.mock.calls[0]?.[1] as RequestInit).signal?.aborted).toBe(
        true,
      );
      await session.refreshMessages("A");
      const newRun = session.sendMessage("A", "new request");
      current.send(textDelta("new answer"), usageEvent(200), {
        type: "compacting",
        data: { triggered_by: "auto" },
      });
      await vi.waitFor(() =>
        expect(session.messages.value.at(-1)?.parts).toEqual([
          { kind: "text", text: "new answer" },
        ]),
      );
      oldResponse.resolve(
        sse([
          textDelta("obsolete answer"),
          usageEvent(9999),
          { type: "title_updated", data: { title: "obsolete title" } },
          {
            type: "approval_requested",
            data: { approval_id: "obsolete", tool: "write_file" },
          },
          cancelledEvent,
        ]),
      );
      await oldRun;
      expect(session.sending.value).toBe(true);
      expect(session.usage.inputTokens.value).toBe(200);
      expect(session.usage.compacting.value).toBe(true);
      expect(useContextUsage().inputTokens.value).toBe(200);
      expect(session.pendingApprovals.value.size).toBe(0);
      expect(titles).toEqual([]);
      expect(session.messages.value.at(-1)?.parts).toEqual([
        { kind: "text", text: "new answer" },
      ]);
      current.send(finishEvent);
      current.close();
      await newRun;
      expect(session.sending.value).toBe(false);
      expect(session.usage.compacting.value).toBe(false);
      expect(useContextUsage().compacting.value).toBe(false);
      expect(
        POST.mock.calls.some(
          (call) => call[0] === "/api/conversations/{id}/cancel",
        ),
      ).toBe(false);
    } finally {
      unsubscribe();
      session.dispose();
    }
  });

  it("旧 finally 已开始的历史刷新迟到时，不能覆盖 B 的新流或解除其 busy", async () => {
    const session = createChatSession({ contextUsage: false });
    const oldHistory = deferred<unknown>();
    GET.mockReturnValueOnce(oldHistory.promise as never);
    const current = openStream();
    vi.stubGlobal(
      "fetch",
      vi
        .fn()
        .mockResolvedValueOnce(sse([textDelta("A"), finishEvent]))
        .mockResolvedValueOnce(current.response),
    );
    const oldRun = session.sendMessage("A", "old");
    await vi.waitFor(() => expect(GET).toHaveBeenCalledTimes(1));
    GET.mockResolvedValue({ data: [] } as never);
    await session.refreshMessages("B");
    const newRun = session.sendMessage("B", "new");
    current.send(textDelta("B live"), usageEvent(222));
    await vi.waitFor(() => expect(session.usage.inputTokens.value).toBe(222));
    oldHistory.resolve({
      data: [storedMessage("assistant", "obsolete stored A")],
      response: new Response(null, {
        headers: { "X-Context-Revision": "99", "X-Context-Tokens": "9999" },
      }),
    });
    await oldRun;
    expect(session.sending.value).toBe(true);
    expect(session.contextRevision.value).toBeNull();
    expect(session.usage.inputTokens.value).toBe(222);
    expect(session.messages.value.at(-1)?.parts).toEqual([
      { kind: "text", text: "B live" },
    ]);
    current.send(finishEvent);
    current.close();
    await newRun;
    session.dispose();
  });

  it("迟到的审批快照不能恢复已决定的子审批，dispose 后也不能写入", async () => {
    const session = createChatSession({ contextUsage: false });
    const approval = {
      approval_id: "worker-approval",
      agent_id: "worker-a",
      call_id: "write-a",
      tool: "write_file",
      kind: "write",
      title: "out.txt",
      run_id: "worker-run",
      path: "out.txt",
      outside: false,
      rule_suggestion: "/tmp/**",
      command: null,
      created_at: "2026-10-04T00:00:00",
    };
    vi.stubGlobal(
      "fetch",
      vi
        .fn()
        .mockResolvedValue(
          sse([{ type: "approval_requested", data: approval }, finishEvent]),
        ),
    );
    GET.mockResolvedValue({ data: [] } as never);
    await session.sendMessage("parent", "delegate");
    expect(session.pendingApprovals.value.get("parent")?.[0]?.agentId).toBe(
      "worker-a",
    );
    const snapshot = deferred<unknown>();
    GET.mockReturnValueOnce(snapshot.promise as never);
    const refresh = session.refreshApprovals("parent");
    POST.mockResolvedValueOnce({
      response: new Response(),
      data: { resolved: true },
    } as never);
    await session.decideApproval("parent", approval.approval_id, "allow");
    snapshot.resolve({ data: [approval] });
    await refresh;
    expect(session.pendingApprovals.value.get("parent")).toEqual([]);
    const afterDispose = deferred<unknown>();
    GET.mockReturnValueOnce(afterDispose.promise as never);
    const disposedRefresh = session.refreshApprovals("parent");
    session.dispose();
    afterDispose.resolve({ data: [approval] });
    await disposedRefresh;
    expect(session.pendingApprovals.value.get("parent")).toEqual([]);
  });
});

describe("execution replay boundary", () => {
  it.each(["conversation", "subagent"] as const)(
    "%s keeps pre-restart tools visible while replaying a newer snapshot",
    async (kind) => {
      const session = createChatSession({ contextUsage: false });
      const history = [
        storedMessage("user", [{ type: "text", text: "original request" }], {
          id: "original-user",
          ordering: 10,
          appended_at_revision: 1,
        }),
        storedMessage(
          "assistant",
          [
            {
              type: "tool_call",
              id: "before-restart",
              name: "list_dir",
              arguments: "{}",
            },
          ],
          { ordering: 30, appended_at_revision: 2 },
        ),
        storedMessage(
          "tool",
          [
            {
              type: "tool_result",
              call_id: "before-restart",
              content: "persisted result",
              is_error: false,
            },
          ],
          {
            id: "copy-of-result",
            ordering: 50,
            appended_at_revision: 3,
            supersedes_message_id: "original-result",
          },
        ),
        storedMessage(
          "assistant",
          [{ type: "text", text: "continued after restart" }],
          { ordering: 70, appended_at_revision: 4 },
        ),
      ];
      GET.mockImplementation(async (path: string) => {
        if (path.endsWith("/run")) return { data: { running: true } } as never;
        if (path.endsWith("/approvals")) return { data: [] } as never;
        return {
          data: history,
          response: new Response(null, {
            headers: { "X-Context-Revision": "4", "X-History-Epoch": "2" },
          }),
        } as never;
      });
      let controller!: ReadableStreamDefaultController<Uint8Array>;
      vi.stubGlobal(
        "fetch",
        vi.fn().mockResolvedValue(
          new Response(
            new ReadableStream<Uint8Array>({
              start(c) {
                controller = c;
              },
            }),
          ),
        ),
      );
      if (kind === "conversation") await session.fetchMessages("recovered");
      const attached =
        kind === "conversation"
          ? session.attachRun("recovered")
          : session.attachSubagent("recovered");
      controller.enqueue(
        new TextEncoder().encode(
          `${[
            {
              type: "run_meta",
              data: {
                run_id: "recovered-run",
                execution_epoch: 2,
                history_epoch: 2,
                replay_after_revision: 3,
                trigger_message_id: "original-user",
              },
            },
            textDelta("continued after restart"),
          ]
            .map((event) => `data: ${JSON.stringify(event)}\n\n`)
            .join("")}`,
        ),
      );
      try {
        await vi.waitFor(() => {
          expect(session.messages.value.at(-1)?.isStreaming).toBe(true);
          expect(session.messages.value.at(-1)?.parts).toContainEqual({
            kind: "text",
            text: "continued after restart",
          });
        });
        const parts = session.messages.value.flatMap(
          (message) => message.parts,
        );
        expect(
          parts.filter(
            (part) =>
              part.kind === "tool_call" && part.callId === "before-restart",
          ),
        ).toHaveLength(1);
        expect(
          parts.filter(
            (part) =>
              part.kind === "text" && part.text === "continued after restart",
          ),
        ).toHaveLength(1);
        expect(session.sending.value).toBe(true);
        expect(session.contextRevision.value).toBeNull();
      } finally {
        controller.enqueue(
          new TextEncoder().encode('data: {"type":"finish"}\n\n'),
        );
        controller.close();
        await attached;
        session.dispose();
      }
    },
  );

  it("reloads history after asynchronous recovery preparation before replaying content", async () => {
    const session = createChatSession({ contextUsage: false });
    const user = storedMessage("user", "request", {
      id: "recovery-user",
      appended_at_revision: 1,
    });
    const repaired = [
      user,
      storedMessage("assistant", "persisted preparation", {
        appended_at_revision: 3,
      }),
    ];
    let historyLoads = 0;
    GET.mockImplementation(async (path: string) => {
      if (path.endsWith("/run")) return { data: { running: true } } as never;
      if (path.endsWith("/approvals")) return { data: [] } as never;
      ++historyLoads;
      return {
        data: historyLoads === 1 ? [user] : repaired,
        response: new Response(null, {
          headers: {
            "X-Context-Revision": historyLoads === 1 ? "1" : "3",
            "X-History-Epoch": "0",
          },
        }),
      } as never;
    });
    let stream!: ReadableStreamDefaultController<Uint8Array>;
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(
        new Response(
          new ReadableStream<Uint8Array>({
            start(c) {
              stream = c;
            },
          }),
        ),
      ),
    );
    await session.fetchMessages("preparing");
    const attached = session.attachRun("preparing");
    stream.enqueue(
      new TextEncoder().encode(
        `data: ${JSON.stringify({ type: "run_meta", data: { run_id: "ready-run", execution_epoch: 1, history_epoch: 0, replay_after_revision: 3, trigger_message_id: user.id } })}\n\ndata: ${JSON.stringify(textDelta("new continuation"))}\n\n`,
      ),
    );
    try {
      await vi.waitFor(() =>
        expect(JSON.stringify(session.messages.value)).toContain(
          "new continuation",
        ),
      );
      expect(JSON.stringify(session.messages.value)).toContain(
        "persisted preparation",
      );
      expect(historyLoads).toBe(2);
    } finally {
      stream.enqueue(new TextEncoder().encode('data: {"type":"finish"}\n\n'));
      stream.close();
      await attached;
      session.dispose();
    }
  });

  it("confirms a send once when ready metadata replaces its optimistic overlay", async () => {
    const session = createChatSession({ contextUsage: false });
    const accepted = vi.fn();
    let requestId = "";
    GET.mockImplementation(
      async () =>
        ({
          data: [
            storedMessage("user", "submitted", {
              id: "durable-user",
              appended_at_revision: 1,
              details: { extra: { client_request_id: requestId } },
            }),
          ],
          response: new Response(null, {
            headers: { "X-Context-Revision": "1", "X-History-Epoch": "0" },
          }),
        }) as never,
    );
    vi.stubGlobal(
      "fetch",
      vi.fn(async (_url, init) => {
        requestId = JSON.parse(init.body).client_request_id;
        return sse([
          {
            type: "run_meta",
            data: {
              run_id: "new-run",
              execution_epoch: 1,
              history_epoch: 0,
              replay_after_revision: 1,
              trigger_message_id: "durable-user",
            },
          },
          textDelta("live response"),
          finishEvent,
        ]);
      }),
    );
    await session.sendMessage(
      "send-ready",
      "submitted",
      null,
      null,
      [],
      accepted,
    );
    expect(accepted).toHaveBeenCalledTimes(1);
    expect(session.messages.value[0]?.id).toBe("durable-user");
    expect(fetch).toHaveBeenCalledTimes(1);
    session.dispose();
  });
});

it("does not mutate approval state when a decision response arrives after disposal", async () => {
  const session = createChatSession({ contextUsage: false });
  GET.mockResolvedValue({ data: [] } as never);
  vi.stubGlobal(
    "fetch",
    vi.fn().mockResolvedValue(
      sse([
        {
          type: "approval_requested",
          data: {
            approval_id: "pending-decision",
            agent_id: null,
            call_id: "write",
            tool: "write_file",
            kind: "write",
            title: "out.txt",
            outside: false,
          },
        },
        finishEvent,
      ]),
    ),
  );
  await session.sendMessage("approval-disposal", "write");
  expect(session.pendingApprovals.value.get("approval-disposal")).toHaveLength(
    1,
  );
  let settle!: (value: unknown) => void;
  POST.mockReturnValue(
    new Promise((resolve) => {
      settle = resolve;
    }) as never,
  );
  const deciding = session.decideApproval(
    "approval-disposal",
    "pending-decision",
    "allow",
  );
  session.dispose();
  settle({ response: new Response(), data: { resolved: true } });
  expect(await deciding).toBe(true);
  expect(session.pendingApprovals.value.get("approval-disposal")).toHaveLength(
    1,
  );
});
