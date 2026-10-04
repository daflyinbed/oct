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
  sendMessage,
  resumeTurn,
  attachRun,
  decideApproval,
  clearMessages,
} = useChat();

// ---------------------------------------------------------------------------
// 测试数据构造
// ---------------------------------------------------------------------------

/** 后端 MessageDto 的最小形状（仅用到本 composable 读的字段）。 */
function storedMessage(
  role: string,
  parts: string | components["schemas"]["MessagePart"][],
  extra: Partial<components["schemas"]["MessageDto"]> = {},
): components["schemas"]["MessageDto"] {
  return {
    id: `msg-${Math.random().toString(36).slice(2)}`,
    role,
    parts: typeof parts === "string" ? legacyParts(parts) : parts,
    details: null,
    ordering: 0,
    created_at: "2026-09-20T00:00:00",
    ...extra,
  };
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
// 重连重放的合成锚点事件（仅 events 端点注入，不进 reducer）
function runMeta(start_message_id: string) {
  return {
    type: "run_meta",
    data: { start_message_id },
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
          String.raw`[{"ToolCall":{"id":"c2","name":"list_dir","arguments":"{}"}}]`,
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
// 实时流（sendMessage → fetch SSE → applyStreamEvent）
// ---------------------------------------------------------------------------

describe("sendMessage 实时流", () => {
  function stubFetch(...responses: Response[]) {
    const fetchMock = vi.fn();
    for (const r of responses) fetchMock.mockResolvedValueOnce(r);
    vi.stubGlobal("fetch", fetchMock);
    return fetchMock;
  }

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
    expect(messages.value[3]?.parts).toEqual([
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
    expect(init).toEqual({ method: "POST" });

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
    // 可暂停的 SSE：读取先挂起，等切换会话后再放行收尾
    const encoder = new TextEncoder();
    let streamController!: ReadableStreamDefaultController<Uint8Array>;
    const stream = new ReadableStream<Uint8Array>({
      start(c) {
        streamController = c;
      },
    });
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

    // 放行 A 的流并结束
    streamController.enqueue(
      encoder.encode(`data: ${JSON.stringify(textDelta("续答"))}\n\n`),
    );
    streamController.enqueue(
      encoder.encode(`data: ${JSON.stringify(finishEvent)}\n\n`),
    );
    streamController.close();
    await resumePromise;

    // 视图仍是 B：收尾刷新被会话守卫拦下（A 的重拉由路由 watch 负责）
    expect(messages.value.map((m) => m.role)).toEqual(["user"]);
    expect(messages.value[0]!.parts).toEqual([{ kind: "text", text: "B会话" }]);
    expect(GET).toHaveBeenCalledTimes(2); // A 占位行 + B 切换加载
    expect(sending.value).toBe(false);
  });

  it("resumeTurn：409 等错误不追加消息、不悬挂 sending，仍刷新历史", async () => {
    stubFetch(new Response("conflict", { status: 409 }));
    GET.mockResolvedValue({ data: danglingHistory() } as never);

    // 先建立视图（恢复按钮只在会话展示时出现），否则收尾刷新的会话
    // 守卫不生效
    const conv = nextConvId();
    await fetchMessages(conv);
    await resumeTurn(conv);

    // 初始加载 + 失败路径的终态刷新共两次 GET；不追加 streaming 消息
    expect(GET).toHaveBeenCalledTimes(2);
    expect(messages.value.map((m) => m.role)).toEqual(["user", "assistant"]);
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
    // 刷新后从 DB 拉到的历史：run 的 user 消息（u2）之后已有轮次边界
    // 落库的第一轮 assistant（a2）——属于 run 之内，重放前必须丢弃。
    stubGetQueue(
      {
        data: [
          storedMessage("user", '[{"Text":"第一问"}]', { id: "u1" }),
          storedMessage("assistant", '[{"Text":"旧答"}]', { id: "a1" }),
          storedMessage("user", '[{"Text":"run 的问题"}]', { id: "u2" }),
          storedMessage(
            "assistant",
            String.raw`[{"ToolCall":{"id":"c1","name":"read_file","arguments":"{}"}}]`,
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
    expect(GET).toHaveBeenCalledTimes(3); // 初始 + 探测 + 静默刷新
    expect(messages.value.at(-1)?.parts).toEqual([
      { kind: "text", text: "答" },
    ]);
    expect(sending.value).toBe(false);
  });

  it("无 run_meta 的兜底：锚点退回最后一条 user 消息（含）", async () => {
    const conv = nextConvId();
    // resume 续跑的 run 没有新 user 消息：DB 尾部是悬空的第一轮 assistant
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

    // 兜底锚到 u1：a1 丢弃，重放从零重建
    expect(messages.value.map((m) => m.id)).toEqual([
      "u1",
      expect.stringMatching(RE_reattachedId),
    ]);
    expect(messages.value[1]!.parts).toEqual([{ kind: "text", text: "续答" }]);
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

    // 放行 A 的余下事件：截断/追加绝不能改写 B 的视图
    sse.send(textDelta("A 的重放"), finishEvent);
    sse.end();
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

    // 关闭标签：dispose 中断订阅（只退订，不取消任务）。mock Response 不
    // 模拟真实 fetch 的 abort 语义，这里手动放行一段"dispose 后到达的
    // 迟到事件 + EOF"，验证 disposed 守卫拦下它们。
    session.dispose();
    const callsAfterDispose = GET.mock.calls.length;
    streamController.enqueue(
      encoder.encode(`data: ${JSON.stringify(textDelta("迟到事件"))}\n\n`),
    );
    streamController.close();
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
        approvalRequested("a1"),
        approvalRequested("a1"), // 重放/重复：同 id 覆盖，不产生第二张卡
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
          String.raw`[{"ToolCall":{"id":"c2","name":"write_file","arguments":"{}"}}]`,
        ),
        storedMessage(
          "tool",
          String.raw`[{"ToolResult":{"call_id":"c2","content":"被拒绝","is_error":true}}]`,
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

describe("failed sends with versioned history", () => {
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
    expect(GET).toHaveBeenCalledTimes(1);

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
      expect(request).toHaveBeenCalledWith("/api/conversations/running/events"),
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
