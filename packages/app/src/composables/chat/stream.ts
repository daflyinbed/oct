import {
  appendLiveOutput,
  MICROCOMPACT_PLACEHOLDER,
  newToolCard,
} from "./display";
import type {
  ChatStreamEvent,
  DisplayPart,
  DisplayWork,
  ToolCallPart,
} from "./display";

export type StreamEffect =
  | { kind: "clear_tool_result"; callId: string; copyId: string }
  | { kind: "compacting"; value: boolean }
  | {
      kind: "usage";
      data: {
        input_tokens?: number | null;
        context_window_tokens?: number | null;
      };
    };

export interface StreamState {
  messageId: string;
  parts: DisplayPart[];
  isStreaming: boolean;
  work: DisplayWork;
  startedAt: number;
  finishedAt?: number;
}

export function newStreamState(
  messageId: string,
  startedAt: number,
): StreamState {
  return {
    messageId,
    parts: [],
    isStreaming: true,
    work: { state: "running" },
    startedAt,
  };
}

export function clonePart(part: DisplayPart): DisplayPart {
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

function findGeneratingCard(
  streamState: StreamState,
  callId: string,
): ToolCallPart | null {
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

function findCard(
  streamState: StreamState,
  callId: string,
): ToolCallPart | null {
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
function closeOpenReasoning(parts: DisplayPart[], now: number): void {
  for (const part of parts) {
    if (part.kind === "reasoning" && !part.ended) {
      part.ended = true;
      if (part.startedAt !== null) {
        part.durationMs = now - part.startedAt;
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
export function finalizeUnconvergedCards(
  streamState: StreamState,
  now: number,
) {
  if (!streamState) return;
  closeOpenReasoning(streamState.parts, now);
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

export function reduceStream(
  streamState: StreamState,
  event: ChatStreamEvent,
  now: number,
): StreamEffect[] {
  const effects: StreamEffect[] = [];
  if (event.type === "subagent_updated") {
    // 正常路径在 consumeSseStream 拦截派发（携带流所属会话 id）；该事件
    // 属会话级任务状态而非消息内容，不进消息 reducer。
    return effects;
  }
  if (!streamState) return effects;
  const parts = streamState.parts;

  // Any non-reasoning event ends the open thinking block (text/tool calls/
  // terminal state all mean the model stopped reasoning).
  if (event.type !== "reasoning_delta") {
    closeOpenReasoning(parts, now);
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
          startedAt: now,
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
      let card = findGeneratingCard(streamState, call_id);
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
      let card = findCard(streamState, id);
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
      const card = findCard(streamState, event.data.call_id);
      if (card) {
        appendLiveOutput(card, event.data.stream, event.data.delta);
      }
      break;
    }

    case "nested_tool_update": {
      const card = findCard(streamState, event.data.parent_call_id);
      if (!card || card.name !== "execute_code") break;
      const previous = Array.isArray(card.details?.calls)
        ? card.details.calls
        : [];
      const calls = previous.filter(
        (row) =>
          row && typeof row === "object" && row.id !== event.data.call.id,
      );
      calls.push(event.data.call);
      card.details = { ...card.details, kind: "code_mode", calls };
      break;
    }

    case "tool_result": {
      const card = findCard(streamState, event.data.call_id);
      if (card) {
        card.status = "done";
        card.content = event.data.content;
        card.isError = event.data.is_error;
        card.details = (event.data.details ?? null) as Record<
          string,
          unknown
        > | null;
        // 权限门拒绝（details kind=permission_denied）：「已拒绝」徽标。
        if (card.details?.kind === "permission_denied") card.denied = true;
      }
      break;
    }

    case "microcompacted": {
      // 旧工具结果被 copy-on-write 替换为占位符：就地折叠工具卡片（所见
      // 即模型所见）；重放幂等（按 call_id 覆盖，值不变）。
      const d = event.data;
      const applyCleared = (card: ToolCallPart, copyId: string) => {
        card.content = MICROCOMPACT_PLACEHOLDER;
        card.cleared = true;
        card.clearedCopyMessageId = copyId;
      };
      for (const r of d.replacements ?? []) {
        // microcompact 在 turn-top 触发且永不触碰最后一轮，目标几乎总在
        // 历史消息的卡片里——通过 effect 交给 timeline 经响应式路径更新。
        // 当前流式消息只改私有 buffer；展示克隆会由 flush 整体覆盖。
        const live = findCard(streamState, r.call_id);
        if (live) applyCleared(live, r.copy_message_id);
        effects.push({
          kind: "clear_tool_result",
          callId: r.call_id,
          copyId: r.copy_message_id,
        });
      }
      break;
    }

    case "finish":
    case "cancelled": {
      streamState.work.state =
        event.type === "finish" ? "completed" : "interrupted";
      if (event.type === "finish") streamState.finishedAt = now;
      finalizeUnconvergedCards(streamState, now);
      streamState.isStreaming = false;
      break;
    }

    case "error": {
      streamState.work.state = "failed";
      const last = parts.at(-1);
      if (last?.kind === "text") {
        last.text += `\n\nError: ${event.data}`;
      } else {
        parts.push({ kind: "text", text: `Error: ${event.data}` });
      }
      finalizeUnconvergedCards(streamState, now);
      streamState.isStreaming = false;
      break;
    }

    case "compacting": {
      // 摘要生成中：状态栏指示器切换为「压缩中」，不进消息流。
      effects.push({ kind: "compacting", value: true });
      break;
    }

    case "compacted": {
      // 事件带全量 summary：当场插入分隔条（此刻通常位于本轮流式消息的
      // 最前部），run 收尾的 loadMessages 会用 stored 标记行接管同一展示。
      effects.push({ kind: "compacting", value: false });
      const d = event.data;
      parts.push({
        kind: "compaction",
        summary: d.summary,
        triggeredBy: d.triggered_by,
        compactedMessages: d.compacted_messages,
        estTokensBefore: d.est_tokens_before,
        estTokensAfter: d.est_tokens_after,
      });
      break;
    }

    case "usage": {
      // 上下文占用指示器数据源（provider 报告的真实请求体积 + 窗口大小）。
      effects.push({ kind: "usage", data: event.data });
      break;
    }
  }
  return effects;
}
