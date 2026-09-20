import { readonly, ref } from "vue";
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
  status: "generating" | "running" | "done";
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

export type DisplayPart = TextPart | ToolCallPart;

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
 * Dedupe guard: the route watch in [id].vue and the tab-return refresh in
 * App.vue can fire for the same conversation within one tick (sidebar
 * switch while a non-chat tab is active); collapse repeats into one request.
 */
let lastFetch: { id: string; at: number } | null = null;

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

  let total = card.liveOutput.droppedChars;
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
      for (const raw of parseStoredParts(m.parts_json)) {
        if (typeof raw.Text === "string") {
          pushText(msg, raw.Text);
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
        // Reasoning/ImageUrl parts are not displayed (unchanged behavior).
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

  return out;
}

// ---------------------------------------------------------------------------
// Live streaming (non-reactive buffers flushed into `messages` on a timer)
// ---------------------------------------------------------------------------

interface StreamState {
  messageId: string;
  parts: (TextPart | ToolCallPart)[];
  isStreaming: boolean;
}

/** Plain (non-reactive) mirror of the streaming message; flushed every tick. */
let streamState: StreamState | null = null;
let flushTimer: ReturnType<typeof setInterval> | null = null;
/** Set by applyStreamEvent; avoids re-rendering unchanged state every tick. */
let streamDirty = false;

const FLUSH_INTERVAL_MS = 50;

function clonePart(part: TextPart | ToolCallPart): DisplayPart {
  if (part.kind === "text") return { ...part };
  return {
    ...part,
    liveOutput: {
      segments: part.liveOutput.segments.map((seg) => ({ ...seg })),
      droppedChars: part.liveOutput.droppedChars,
    },
  };
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
 * Terminal events may leave cards that never reached tool_result: drop
 * still-generating cards (never dispatched and not persisted — dropping keeps
 * the live view identical to history replay) and settle running ones as
 * errored so no spinner pulses forever. Idempotent; also called from the
 * sendMessage finally block to cover streams that die without a terminal
 * event (e.g. connection reset). Cancellation is covered here too: the
 * backend persists a ToolResult for cancelled calls but does not stream one.
 */
function finalizeUnconvergedCards() {
  if (!streamState) return;
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
  // Mark dirty even for ignored events (usage/reasoning_delta): one extra
  // 50ms flush at most, and far simpler than tracking per-branch mutations.
  streamDirty = true;
  const parts = streamState.parts;

  switch (event.type) {
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

    // usage / reasoning_delta: intentionally not displayed (unchanged).
  }
}

// ---------------------------------------------------------------------------

export function useChat() {
  const fetchMessages = async (conversationId: string) => {
    const now = Date.now();
    if (
      lastFetch &&
      lastFetch.id === conversationId &&
      now - lastFetch.at < 200
    ) {
      return;
    }
    lastFetch = { id: conversationId, at: now };
    const { data, error } = await client.GET(
      "/api/conversations/{id}/messages",
      { params: { path: { id: conversationId } } },
    );
    if (!error && data) {
      messages.value = storedToDisplayList(data);
    }
  };

  const sendMessage = async (
    conversationId: string,
    content: string,
    providerSpec?: string | null,
  ) => {
    if (!content.trim() || sending.value) return;
    sending.value = true;

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
      const baseUrl = "http://127.0.0.1:3000";
      const body: { content: string; provider_spec?: string } = { content };
      if (providerSpec) body.provider_spec = providerSpec;
      const res = await fetch(
        `${baseUrl}/api/conversations/${conversationId}/messages`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(body),
        },
      );

      if (!res.ok || !res.body) {
        throw new Error(`HTTP ${res.status}`);
      }

      const reader = res.body.getReader();
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

          try {
            const event: AgentEvent = JSON.parse(payload);
            applyStreamEvent(event);
          } catch {
            // skip malformed SSE
          }
        }
      }
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
      sending.value = false;
    }
  };

  const clearMessages = () => {
    messages.value = [];
  };

  return {
    messages: readonly(messages),
    sending: readonly(sending),
    fetchMessages,
    sendMessage,
    clearMessages,
  };
}
