import { computed, readonly, ref } from "vue";
import client from "@/api/client";
import type { components } from "@/api/schema";

type StoredMessage = components["schemas"]["StoredMessage"];
type AgentEvent = components["schemas"]["AgentEvent"];
type OutputStream = components["schemas"]["OutputStream"];

/** One continuous run of a single stream inside the live output pane. */
export interface LiveOutputSegment {
  stream: OutputStream;
  text: string;
}

export interface TextPart {
  kind: "text";
  text: string;
}

export interface ToolCallPart {
  kind: "tool_call";
  callId: string;
  /** Tool name; null while the first tool_call_delta has not arrived. */
  name: string | null;
  /** Human-readable title from tool_call_start (e.g. the command line). */
  title: string | null;
  /** Accumulated (generating) or canonical (tool_call_start onwards) JSON text. */
  arguments: string;
  /**
   * "interrupted"：历史里始终没有等到配对 ToolResult 的调用（后端中断，
   * 如崩溃重启），由 storedToDisplayList 标注。
   */
  status: "generating" | "running" | "done" | "interrupted";
  /**
   * Live execution output; never persisted, live view only. Segments keep the
   * arrival order of stdout/stderr chunks (capped to the tail). Readonly so
   * DeepReadonly display messages stay assignable to these interfaces; the
   * stream buffer mutates it via a cast.
   */
  liveOutput: { segments: readonly LiveOutputSegment[]; droppedChars: number };
  /** Final model-visible content (post-truncation), set by tool_result. */
  content: string | null;
  /** Structured per-tool metadata, set by tool_result / history details_json. */
  details: Record<string, unknown> | null;
  isError: boolean;
}

export interface ReasoningPart {
  kind: "reasoning";
  text: string;
  /** Epoch ms of the first delta; null for history-replayed parts. */
  startedAt: number | null;
  /**
   * Thinking duration in ms; null while the block is still receiving deltas
   * or when a history row has no stored duration.
   */
  durationMs: number | null;
  /** False only while the live block is still receiving deltas. */
  ended: boolean;
}

export type DisplayPart = TextPart | ToolCallPart | ReasoningPart;

export interface DisplayMessage {
  id: string;
  role: "user" | "assistant";
  parts: readonly DisplayPart[];
  isStreaming?: boolean;
}

/** Internal mutable shape used while building/merging messages. */
interface BuildableMessage {
  id: string;
  role: "user" | "assistant";
  parts: DisplayPart[];
  isStreaming?: boolean;
}

const messages = ref<DisplayMessage[]>([]);
const sending = ref(false);

/**
 * title_updated 事件监听器。标题是会话元数据而非消息内容：在 SSE 消费层
 * 拦截后直接派发给宿主（App.vue 接线到 useProjects 的会话列表），不进
 * applyStreamEvent 的消息 reducer，也不受"视图是否停留本会话"的守卫影响
 * ——侧栏里其他会话的标题更新同样要反映出来。
 */
const titleListeners = new Set<
  (conversationId: string, title: string) => void
>();

/** 订阅 title_updated 事件；返回取消订阅函数。 */
export function onTitleUpdated(
  listener: (conversationId: string, title: string) => void,
): () => void {
  titleListeners.add(listener);
  return () => titleListeners.delete(listener);
}

/**
 * Dedupe guard: the route watch in [id].vue and the tab-return refresh in
 * App.vue can fire for the same conversation within one tick (sidebar
 * switch while a non-chat tab is active); collapse repeats into one request.
 */
let lastFetch: { id: string; at: number } | null = null;

/**
 * `messages` 当前展示的会话 id（null = 空态）。流式收尾的静默刷新以它
 * 判断视图是否仍在该会话上：resume/send 进行中用户可能已切走，无条件
 * 刷新会用旧会话的历史覆盖当前视图。
 */
let activeConversationId: string | null = null;

/** Tail cap per tool call for live output kept in the DOM. */
const LIVE_OUTPUT_MAX_CHARS = 32_768;

// ---------------------------------------------------------------------------
// History replay
// ---------------------------------------------------------------------------

interface StoredToolCallShape {
  id: string;
  name: string;
  arguments: string;
}

interface StoredToolResultShape {
  call_id: string;
  content: unknown;
  is_error: boolean;
}

/** parts_json uses serde's externally tagged representation: {"Text": "..."} etc. */
function parseStoredParts(partsJson: string): Record<string, unknown>[] {
  try {
    const parts = JSON.parse(partsJson);
    if (Array.isArray(parts)) return parts;
  } catch {
    // Legacy/invalid payload: surfaced as a single text part below.
  }
  return [{ Text: partsJson }];
}

function newToolCard(callId: string, name: string | null): ToolCallPart {
  return {
    kind: "tool_call",
    callId,
    name,
    title: null,
    arguments: "",
    status: "generating",
    liveOutput: { segments: [], droppedChars: 0 },
    content: null,
    details: null,
    isError: false,
  };
}

/**
 * Append a live output delta, merging into the trailing segment when the
 * stream is unchanged, then trim past the tail cap (oldest segments first).
 */
function appendLiveOutput(
  card: ToolCallPart,
  stream: OutputStream,
  delta: string,
) {
  // The public type is readonly (see ToolCallPart.liveOutput); the stream
  // buffer is plain mutable data.
  const segments = card.liveOutput.segments as LiveOutputSegment[];
  const last = segments.at(-1);
  if (last && last.stream === stream) {
    last.text += delta;
  } else {
    segments.push({ stream, text: delta });
  }

  // The cap applies to RETAINED text only; droppedChars is a running count
  // of what was trimmed, not part of the budget. (Including it here made the
  // loop over-trim: once droppedChars alone exceeded the cap, every further
  // delta was sliced away until the live view emptied out.)
  let total = 0;
  for (const seg of segments) total += seg.text.length;
  while (total > LIVE_OUTPUT_MAX_CHARS && segments.length > 0) {
    const first = segments[0];
    if (!first) break;
    if (segments.length === 1) {
      // Single-stream calls merge into one segment, which the drop-whole-
      // segment path below could never trim: slice its head instead.
      const excess = total - LIVE_OUTPUT_MAX_CHARS;
      if (first.text.length <= excess) break;
      first.text = first.text.slice(excess);
      card.liveOutput.droppedChars += excess;
      total = LIVE_OUTPUT_MAX_CHARS;
    } else {
      segments.shift();
      total -= first.text.length;
      card.liveOutput.droppedChars += first.text.length;
    }
  }
}

function pushText(msg: BuildableMessage, text: string) {
  const last = msg.parts.at(-1);
  if (last?.kind === "text") {
    last.text += text;
  } else {
    msg.parts.push({ kind: "text", text });
  }
}

function detailsFromJson(value: string | null): Record<string, unknown> | null {
  if (!value) return null;
  try {
    const parsed = JSON.parse(value);
    return parsed && typeof parsed === "object" && !Array.isArray(parsed)
      ? (parsed as Record<string, unknown>)
      : null;
  } catch {
    return null;
  }
}

/**
 * Build display messages from stored history. Tool messages are not rendered
 * as bubbles: their ToolResult parts fill the matching tool card (which may
 * live in an earlier assistant message), re-applying details_json so cards
 * re-render identically to the live stream. Orphan results (old data without
 * a matching card) become a minimal done card named "unknown".
 */
function storedToDisplayList(stored: StoredMessage[]): DisplayMessage[] {
  const out: BuildableMessage[] = [];
  const cardsByCallId = new Map<string, ToolCallPart>();
  let lastAssistant: BuildableMessage | null = null;

  const fillCard = (
    card: ToolCallPart,
    result: StoredToolResultShape,
    details: Record<string, unknown> | null,
  ) => {
    card.status = "done";
    card.content =
      typeof result.content === "string"
        ? result.content
        : JSON.stringify(result.content, null, 2);
    card.isError = result.is_error;
    if (!card.title && typeof details?.title === "string") {
      card.title = details.title;
    }
    card.details = details;
  };

  for (const m of stored) {
    if (m.role === "user") {
      const msg: BuildableMessage = { id: m.id, role: "user", parts: [] };
      for (const raw of parseStoredParts(m.parts_json)) {
        if (typeof raw.Text === "string") pushText(msg, raw.Text);
      }
      out.push(msg);
      lastAssistant = null;
      continue;
    }

    if (m.role === "assistant") {
      const msg: BuildableMessage = { id: m.id, role: "assistant", parts: [] };
      // Thinking duration is UI metadata persisted alongside the message
      // (details_json), not a message part.
      const details = detailsFromJson(m.details_json ?? null);
      const storedDurationMs =
        typeof details?.reasoning_duration_ms === "number"
          ? details.reasoning_duration_ms
          : null;
      for (const raw of parseStoredParts(m.parts_json)) {
        if (typeof raw.Text === "string") {
          pushText(msg, raw.Text);
        } else if (typeof raw.Reasoning === "string") {
          msg.parts.push({
            kind: "reasoning",
            text: raw.Reasoning,
            startedAt: null,
            durationMs: storedDurationMs,
            ended: true,
          });
        } else if (raw.ToolCall && typeof raw.ToolCall === "object") {
          const call = raw.ToolCall as StoredToolCallShape;
          const card = newToolCard(call.id, call.name);
          card.arguments = call.arguments ?? "";
          // Stored calls have finished executing; the matching tool message
          // (if any) fills in content/details below.
          card.status = "done";
          msg.parts.push(card);
          cardsByCallId.set(card.callId, card);
        }
        // ImageUrl parts are not displayed (no UI for them).
      }
      out.push(msg);
      lastAssistant = msg;
      continue;
    }

    if (m.role === "tool") {
      const details = detailsFromJson(m.details_json ?? null);
      for (const raw of parseStoredParts(m.parts_json)) {
        if (!raw.ToolResult || typeof raw.ToolResult !== "object") continue;
        const result = raw.ToolResult as StoredToolResultShape;
        const card = cardsByCallId.get(result.call_id);
        if (card) {
          fillCard(card, result, details);
        } else {
          // Orphan result (e.g. data from before cards were stored): host a
          // minimal done card on the most recent assistant message.
          if (!lastAssistant) {
            lastAssistant = { id: m.id, role: "assistant", parts: [] };
            out.push(lastAssistant);
          }
          const orphan = newToolCard(result.call_id, "unknown");
          orphan.status = "done";
          lastAssistant.parts.push(orphan);
          fillCard(orphan, result, details);
        }
      }
    }
  }

  // 悬空调用：始终没等到配对 ToolResult（fillCard 未被调用，content 仍为
  // null）的卡片标注为已中断——后端崩溃留下的历史，恢复前 UI 的可见信号。
  for (const card of cardsByCallId.values()) {
    if (card.content === null) card.status = "interrupted";
  }

  return out;
}

// ---------------------------------------------------------------------------
// Live streaming (non-reactive buffers flushed into `messages` on a timer)
// ---------------------------------------------------------------------------

interface StreamState {
  messageId: string;
  parts: DisplayPart[];
  isStreaming: boolean;
}

/** Plain (non-reactive) mirror of the streaming message; flushed every tick. */
let streamState: StreamState | null = null;
let flushTimer: ReturnType<typeof setInterval> | null = null;
/** Set by applyStreamEvent; avoids re-rendering unchanged state every tick. */
let streamDirty = false;

const FLUSH_INTERVAL_MS = 50;

function clonePart(part: DisplayPart): DisplayPart {
  if (part.kind === "tool_call") {
    return {
      ...part,
      liveOutput: {
        segments: part.liveOutput.segments.map((seg) => ({ ...seg })),
        droppedChars: part.liveOutput.droppedChars,
      },
    };
  }
  return { ...part };
}

/** Write the buffered stream state into the reactive messages array. */
function flushStream() {
  if (!streamState || !streamDirty) return;
  streamDirty = false;
  const idx = messages.value.findIndex((m) => m.id === streamState?.messageId);
  if (idx === -1) return;
  messages.value[idx] = {
    ...messages.value[idx],
    parts: streamState.parts.map(clonePart),
    isStreaming: streamState.isStreaming,
  };
}

function startFlushing() {
  stopFlushing();
  flushTimer = setInterval(flushStream, FLUSH_INTERVAL_MS);
}

function stopFlushing() {
  if (flushTimer !== null) {
    clearInterval(flushTimer);
    flushTimer = null;
  }
}

function findGeneratingCard(callId: string): ToolCallPart | null {
  if (!streamState) return null;
  for (let i = streamState.parts.length - 1; i >= 0; i--) {
    const part = streamState.parts[i] as ToolCallPart;
    if (
      part.kind === "tool_call" &&
      part.status === "generating" &&
      part.callId === callId
    ) {
      return part;
    }
  }
  return null;
}

function findCard(callId: string): ToolCallPart | null {
  if (!streamState) return null;
  for (let i = streamState.parts.length - 1; i >= 0; i--) {
    const part = streamState.parts[i] as ToolCallPart;
    if (part.kind === "tool_call" && part.callId === callId) {
      return part;
    }
  }
  return null;
}

/**
 * Close any still-open reasoning block, stamping its duration. Only the last
 * part can be open, but scanning all parts keeps this idempotent.
 */
function closeOpenReasoning(parts: DisplayPart[]): void {
  for (const part of parts) {
    if (part.kind === "reasoning" && !part.ended) {
      part.ended = true;
      if (part.startedAt !== null) {
        part.durationMs = Date.now() - part.startedAt;
      }
    }
  }
}

/**
 * Terminal events may leave cards that never reached tool_result: drop
 * still-generating cards (never dispatched and not persisted — dropping keeps
 * the live view identical to history replay) and settle running ones as
 * errored so no spinner pulses forever. An open thinking block is closed with
 * its duration so the card stops showing its streaming state. Idempotent;
 * also called from the sendMessage finally block to cover streams that die
 * without a terminal event (e.g. connection reset). Cancellation is covered
 * here too: the backend persists a ToolResult for cancelled calls but does
 * not stream one.
 */
function finalizeUnconvergedCards() {
  if (!streamState) return;
  closeOpenReasoning(streamState.parts);
  streamState.parts = streamState.parts.filter(
    (part) => !(part.kind === "tool_call" && part.status === "generating"),
  );
  for (const part of streamState.parts) {
    if (part.kind === "tool_call" && part.status === "running") {
      part.status = "done";
      part.isError = true;
    }
  }
}

function applyStreamEvent(event: AgentEvent) {
  if (!streamState) return;
  // Mark dirty even for ignored events (usage): one extra 50ms flush at
  // most, and far simpler than tracking per-branch mutations.
  streamDirty = true;
  const parts = streamState.parts;

  // Any non-reasoning event ends the open thinking block (text/tool calls/
  // terminal state all mean the model stopped reasoning).
  if (event.type !== "reasoning_delta") {
    closeOpenReasoning(parts);
  }

  switch (event.type) {
    case "reasoning_delta": {
      const last = parts.at(-1);
      if (last?.kind === "reasoning" && !last.ended) {
        last.text += event.data;
      } else {
        // A new agent-loop round opens a fresh thinking block.
        parts.push({
          kind: "reasoning",
          text: event.data,
          startedAt: Date.now(),
          durationMs: null,
          ended: false,
        });
      }
      break;
    }

    case "text_delta": {
      const last = parts.at(-1);
      if (last?.kind === "text") {
        last.text += event.data;
      } else {
        parts.push({ kind: "text", text: event.data });
      }
      break;
    }

    case "tool_call_delta": {
      const { call_id, name, arguments_delta } = event.data;
      let card = findGeneratingCard(call_id);
      if (!card) {
        card = newToolCard(call_id, name ?? null);
        parts.push(card);
      } else if (name && !card.name) {
        card.name = name;
      }
      card.arguments += arguments_delta;
      break;
    }

    case "tool_call_start": {
      const { id, name, title } = event.data;
      // Match any existing card for this id first (a repeated start
      // re-syncs it instead of spawning a duplicate); only then fall back
      // to adopting an unmatched generating card.
      let card = findCard(id);
      if (!card) {
        // The delta-phase call_id may differ from the dispatch id (e.g.
        // Anthropic synthetic ids): adopt an unmatched generating card with
        // the same tool name, in order; a still-unnamed card is the next
        // best candidate.
        for (const part of parts) {
          if (
            part.kind === "tool_call" &&
            part.status === "generating" &&
            part.name === name
          ) {
            card = part;
            break;
          }
        }
        if (!card) {
          for (const part of parts) {
            if (
              part.kind === "tool_call" &&
              part.status === "generating" &&
              !part.name
            ) {
              card = part;
              break;
            }
          }
        }
      }
      if (!card) {
        card = newToolCard(id, name);
        parts.push(card);
      }
      card.callId = id;
      card.name = name;
      card.title = title;
      // Canonical arguments replace the accumulated delta text (also
      // self-heals dropped deltas).
      card.arguments = event.data.arguments;
      card.status = "running";
      break;
    }

    case "tool_output_delta": {
      const card = findCard(event.data.call_id);
      if (card) {
        appendLiveOutput(card, event.data.stream, event.data.delta);
      }
      break;
    }

    case "tool_result": {
      const card = findCard(event.data.call_id);
      if (card) {
        card.status = "done";
        card.content = event.data.content;
        card.isError = event.data.is_error;
        card.details = (event.data.details ?? null) as Record<
          string,
          unknown
        > | null;
      }
      break;
    }

    case "finish":
    case "cancelled": {
      finalizeUnconvergedCards();
      streamState.isStreaming = false;
      break;
    }

    case "error": {
      const last = parts.at(-1);
      if (last?.kind === "text") {
        last.text += `\n\nError: ${event.data}`;
      } else {
        parts.push({ kind: "text", text: `Error: ${event.data}` });
      }
      finalizeUnconvergedCards();
      streamState.isStreaming = false;
      break;
    }

    // usage: intentionally not displayed.
  }
}

// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// SSE 消费（sendMessage / resumeTurn 共用）
// ---------------------------------------------------------------------------

/**
 * 读取一个 SSE Response 直到流 EOF，把每个 data: 事件交给回调（默认
 * applyStreamEvent；attachRun 借此拦截 run_meta）。title_updated 是会话
 * 元数据更新，在此直接派发给监听器、不进回调。网络分片可能在任意
 * 字节边界断开（含 data: 前缀与 JSON 中间），逐行缓冲解析。
 */
async function consumeSseStream(
  res: Response,
  conversationId: string,
  onEvent: (event: AgentEvent) => void = applyStreamEvent,
): Promise<void> {
  const reader = res.body!.getReader();
  const decoder = new TextDecoder();
  let buffer = "";

  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });

    const lines = buffer.split("\n");
    buffer = lines.pop() ?? "";

    for (const line of lines) {
      if (!line.startsWith("data: ")) continue;
      const payload = line.slice(6).trim();
      if (payload === "[DONE]") continue;

      let event: AgentEvent;
      try {
        event = JSON.parse(payload);
      } catch {
        // skip malformed SSE
        continue;
      }
      if (event.type === "title_updated") {
        // 派发放在 parse 的 try 之外：监听器异常不该被当成坏帧吞掉，
        // 也不该中断其余监听器收到本事件。
        for (const listener of titleListeners) {
          listener(conversationId, event.data.title);
        }
        continue;
      }
      onEvent(event);
    }
  }
}

// ---------------------------------------------------------------------------

export function useChat() {
  /** 实际拉取并重建 messages（绕过 fetchMessages 的 200ms 去重守卫）。 */
  const loadMessages = async (conversationId: string) => {
    lastFetch = { id: conversationId, at: Date.now() };
    const { data, error } = await client.GET(
      "/api/conversations/{id}/messages",
      { params: { path: { id: conversationId } } },
    );
    if (!error && data) {
      messages.value = storedToDisplayList(data);
      activeConversationId = conversationId;
    }
  };

  const fetchMessages = async (conversationId: string) => {
    const now = Date.now();
    if (
      lastFetch &&
      lastFetch.id === conversationId &&
      now - lastFetch.at < 200
    ) {
      return;
    }
    await loadMessages(conversationId);
  };

  const sendMessage = async (
    conversationId: string,
    content: string,
    providerSpec?: string | null,
  ) => {
    if (!content.trim() || sending.value) return;
    sending.value = true;

    // 后端在 send 时会顺带修复悬空调用并落占位行：发送前若存在中断卡片，
    // 结束后需刷新一次历史让占位结果回填，否则 interrupted/resumable
    // 状态过期（按钮残留，点击只得静默 409）。
    const hadInterruptedCards = messages.value.some((m) =>
      m.parts.some((p) => p.kind === "tool_call" && p.status === "interrupted"),
    );

    messages.value = [
      ...messages.value,
      {
        id: `pending-${Date.now()}`,
        role: "user",
        parts: [{ kind: "text", text: content }],
      },
    ];

    const assistantId = `stream-${Date.now()}`;
    messages.value = [
      ...messages.value,
      { id: assistantId, role: "assistant", parts: [], isStreaming: true },
    ];
    streamState = { messageId: assistantId, parts: [], isStreaming: true };
    startFlushing();

    try {
      const body: { content: string; provider_spec?: string } = { content };
      if (providerSpec) body.provider_spec = providerSpec;
      const res = await fetch(`/api/conversations/${conversationId}/messages`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      });

      if (!res.ok || !res.body) {
        throw new Error(`HTTP ${res.status}`);
      }

      await consumeSseStream(res, conversationId);
    } catch {
      if (streamState) {
        streamState.parts.push({
          kind: "text",
          text: "Failed to send message.",
        });
        streamDirty = true;
      }
    } finally {
      stopFlushing();
      if (streamState) {
        finalizeUnconvergedCards();
        streamState.isStreaming = false;
        flushStream();
        streamState = null;
      }
      // 刷新失败不影响流式终态（下次 fetch 自会收敛），静默即可。
      if (hadInterruptedCards && activeConversationId === conversationId) {
        try {
          await loadMessages(conversationId);
        } catch {
          // ignore: cosmetic refresh
        }
      }
      sending.value = false;
    }
  };

  /**
   * 恢复被中断的轮次（后端崩溃等）：修复悬空工具调用后不追加 user 消息，
   * 直接以现有历史续跑 agent loop。发送中（sending）时忽略。
   */
  const resumeTurn = async (conversationId: string) => {
    if (sending.value) return;
    sending.value = true;

    try {
      const res = await fetch(`/api/conversations/${conversationId}/resume`, {
        method: "POST",
      });

      if (!res.ok || !res.body) {
        throw new Error(`HTTP ${res.status}`);
      }

      // 后端已把占位 tool 行持久化：先拉取让悬空卡片落定，再开始消费
      // 事件（SSE 事件在开始读取前由浏览器缓冲，不会丢失）。
      await loadMessages(conversationId);

      const assistantId = `resumed-${Date.now()}`;
      messages.value = [
        ...messages.value,
        { id: assistantId, role: "assistant", parts: [], isStreaming: true },
      ];
      streamState = { messageId: assistantId, parts: [], isStreaming: true };
      startFlushing();

      await consumeSseStream(res, conversationId);
    } catch {
      if (streamState) {
        streamState.parts.push({
          kind: "text",
          text: "Failed to resume turn.",
        });
        streamDirty = true;
      }
    } finally {
      stopFlushing();
      if (streamState) {
        finalizeUnconvergedCards();
        streamState.isStreaming = false;
        flushStream();
        streamState = null;
      }
      // 静默刷新：以服务端持久化的终态为准（也顺带清掉过期的可恢复标记）。
      // 仅当视图仍停留在本会话：切走后由路由 watch 负责重新拉取，这里
      // 不能用旧会话的历史覆盖当前视图。刷新失败不影响终态，静默即可。
      if (activeConversationId === conversationId) {
        try {
          await loadMessages(conversationId);
        } catch {
          // ignore: cosmetic refresh
        }
      }
      sending.value = false;
    }
  };

  /**
   * 重连运行中的 run（前端刷新后）：先探测 /run，运行中则订阅 /events，
   * 从 run 起点完整重放事件流，用现有 reducer 重建视图后继续实时增长。
   * run 的 user 消息行是 DB 历史与事件流的分界点：重放前先按 run_meta
   * 锚点截断 messages（保留锚点及之前的所有消息、丢弃之后的），保证
   * "DB 历史 + 事件重放"恰好拼出完整视图、不重不漏。与 resumeTurn
   * （后端崩溃后的续跑）不同：run 仍在本进程内跑着，内存事件日志即可
   * 覆盖全部事件。终态/EOF/失败统一走 finally 的静默刷新收敛。
   */
  const attachRun = async (conversationId: string) => {
    if (sending.value) return;
    sending.value = true;

    try {
      // 先探状态（404/请求失败按空闲处理，交给 finally 的静默刷新兜底）。
      const { data, error } = await client.GET("/api/conversations/{id}/run", {
        params: { path: { id: conversationId } },
      });
      if (error || !data?.running) {
        // 空闲：静默刷新后解锁返回（也兜住"查状态与订阅之间刚好结束"
        // 的竞态——刷新拿到 DB 终态）。
        return;
      }

      const res = await fetch(`/api/conversations/${conversationId}/events`);
      if (!res.ok || !res.body) {
        // 404：探测与订阅之间 run 结束——回到 DB 历史（finally 刷新）。
        throw new Error(`HTTP ${res.status}`);
      }

      // 截断锚点：run_meta.start_message_id 对应的 user 消息行；后端没记
      // 录（如 resume 续跑的 run，无新 user 消息）时兜底锚到最后一条
      // user 消息。截断必须在应用任何 delta 之前完成，因此放在注入回调
      // 里、随首个事件执行恰好一次。
      let truncated = false;
      const lastUserIndex = () => {
        for (let i = messages.value.length - 1; i >= 0; i--) {
          if (messages.value[i]?.role === "user") return i;
        }
        // 没有 user 消息（异常历史）：无从截断，全部保留。
        return messages.value.length - 1;
      };
      const truncate = (anchorIndex: number) => {
        messages.value = messages.value.slice(0, anchorIndex + 1);
        const assistantId = `reattached-${Date.now()}`;
        messages.value = [
          ...messages.value,
          { id: assistantId, role: "assistant", parts: [], isStreaming: true },
        ];
        streamState = { messageId: assistantId, parts: [], isStreaming: true };
        startFlushing();
      };

      await consumeSseStream(res, conversationId, (event) => {
        // 会话守卫：切走后（activeConversationId 已变）本流的事件不再进
        // 视图——尤其截断会改写 messages，绝不能落在别的会话头上。收尾
        // 刷新同样被守卫拦下，旧会话的重拉由路由 watch 负责。
        if (activeConversationId !== conversationId) return;
        if (!truncated) {
          if (event.type === "run_meta") {
            const idx = messages.value.findIndex(
              (m) => m.id === event.data.start_message_id,
            );
            truncate(idx !== -1 ? idx : lastUserIndex());
            truncated = true;
            return; // 合成事件，不进 reducer
          }
          truncate(lastUserIndex());
          truncated = true;
        }
        applyStreamEvent(event);
      });
    } catch {
      // 网络失败：静默（与 resumeTurn 一致的最小处理），finally 统一收敛。
    } finally {
      stopFlushing();
      if (streamState) {
        finalizeUnconvergedCards();
        streamState.isStreaming = false;
        flushStream();
        streamState = null;
      }
      // 静默刷新：以服务端持久化的终态为准。仅当视图仍停留在本会话——
      // 切走后由路由 watch 负责重新拉取，这里不能用旧会话的历史覆盖
      // 当前视图。刷新失败不影响终态，静默即可。
      if (activeConversationId === conversationId) {
        try {
          await loadMessages(conversationId);
        } catch {
          // ignore: cosmetic refresh
        }
      }
      sending.value = false;
    }
  };

  /**
   * 请求取消该会话正在运行的 agent。UI 状态由 SSE 流（cancelled 事件 +
   * 流关闭）落定而非本响应；错误（如 run 已结束时的 404）忽略即可。
   */
  const cancelConversation = async (conversationId: string) => {
    await client.POST("/api/conversations/{id}/cancel", {
      params: { path: { id: conversationId } },
    });
  };

  const clearMessages = () => {
    messages.value = [];
    activeConversationId = null;
  };

  /** 存在被中断的轮次可恢复：悬空工具卡片，或最后一条消息是 user。 */
  const resumable = computed(() => {
    if (messages.value.length === 0) return false;
    if (messages.value.at(-1)?.role === "user") {
      return true;
    }
    return messages.value.some((m) =>
      m.parts.some((p) => p.kind === "tool_call" && p.status === "interrupted"),
    );
  });

  return {
    messages: readonly(messages),
    sending: readonly(sending),
    resumable,
    fetchMessages,
    sendMessage,
    resumeTurn,
    attachRun,
    cancelConversation,
    clearMessages,
  };
}
