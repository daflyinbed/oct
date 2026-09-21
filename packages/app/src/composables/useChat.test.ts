import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import client from "@/api/client";
import { useChat } from "@/composables/useChat";

import type { ReasoningPart, ToolCallPart } from "@/composables/useChat";
import type { DeepReadonly } from "vue";

// fetchMessages 走 openapi-fetch client，这里整体 mock 掉。
vi.mock("@/api/client", () => ({
  default: { GET: vi.fn(), POST: vi.fn() },
}));

const GET = vi.mocked(client.GET);

const {
  messages,
  sending,
  resumable,
  fetchMessages,
  sendMessage,
  resumeTurn,
  attachRun,
  clearMessages,
} = useChat();

// ---------------------------------------------------------------------------
// 测试数据构造
// ---------------------------------------------------------------------------

/** 后端 StoredMessage 的最小形状（仅用到本 composable 读的字段）。 */
function storedMessage(
  role: string,
  partsJson: string,
  extra: Record<string, unknown> = {},
) {
  return {
    id: `msg-${Math.random().toString(36).slice(2)}`,
    conversation_id: "conv",
    role,
    parts_json: partsJson,
    details_json: null,
    ordering: 0,
    provider_id: null,
    model_id: null,
    input_tokens: null,
    output_tokens: null,
    reasoning_tokens: null,
    created_at: "2026-09-20T00:00:00",
    ...extra,
  };
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
  GET.mockReset();
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
          { details_json: '{"title":"a.txt","total_lines":1}' },
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

  it("历史里的 Reasoning part 渲染为思考卡片，时长来自 details_json", async () => {
    GET.mockResolvedValue({
      data: [
        storedMessage("user", '[{"Text":"问"}]'),
        storedMessage("assistant", '[{"Reasoning":"历史思考"},{"Text":"答"}]', {
          details_json: '{"reasoning_duration_ms":4200}',
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

  /** GET 队列：初始历史 → /run 探测 →（终态）静默刷新。 */
  function stubGetQueue(...results: unknown[]) {
    for (const r of results) GET.mockResolvedValueOnce(r as never);
  }

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
    // 订阅打到 events 端点；GET 轨迹：初始加载 → /run 探测 → 终态刷新
    expect(sse.fetchMock.mock.calls[0]?.[0]).toBe(
      `/api/conversations/${conv}/events`,
    );
    expect(GET).toHaveBeenCalledTimes(3);
    expect(GET.mock.calls[1]?.[0]).toBe("/api/conversations/{id}/run");
    expect(GET.mock.calls[2]?.[0]).toBe("/api/conversations/{id}/messages");
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
    expect(GET).toHaveBeenCalledTimes(3);
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
    );
    const sse = pausableSse();

    await fetchMessages(convA);
    const attachPromise = attachRun(convA);
    sse.send(runMeta("u2"));
    await vi.waitFor(() =>
      expect(messages.value.some((m) => RE_reattachedId.test(m.id))).toBe(true),
    );

    // 切换到会话 B（GET#3：B 的加载）
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
    expect(GET).toHaveBeenCalledTimes(3); // A 初始 + A 探测 + B 切换（A 收尾刷新被拦）
    expect(sending.value).toBe(false);
  });
});
