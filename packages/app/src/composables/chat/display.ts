import type { components } from "@/api/schema";

export type ApiMessage = components["schemas"]["MessageDto"];
export type ForkCut = components["schemas"]["ForkCut"];
export type ApiMessagePart = components["schemas"]["MessagePart"];
export type ApiMessageDetails = components["schemas"]["MessageDetails"];
export type AgentEvent = components["schemas"]["AgentEvent"];
export type OutputStream = components["schemas"]["OutputStream"];
export type PendingApprovalDto = components["schemas"]["PendingApprovalDto"];

/** The SSE event union, straight from the generated OpenAPI schema. */
export type ChatStreamEvent = AgentEvent;

/** 审批决定（decide 端点的 decision 字段词表）。 */
export type ApprovalDecision = "allow" | "allow_always" | "deny";

/**
 * 会话内待决审批：`approval_requested` 事件与 pending
 * 快照端点（GET /approvals）两路归一的展示形状。审批内存即焚，前端只
 * 维护 pending 集合——resolved 事件/decide 响应即移除，无持久化。
 */
export interface PendingApproval {
  approvalId: string;
  /** 被挂起的工具调用 id（审批卡与消息流里的工具卡对应关系）。 */
  callId: string;
  /** 模型侧工具名（如 `write_file` / `mcp__server__tool`）。 */
  tool: string;
  /** 动作类别（`read`/`write`/`edit`/`command`/`mcp`）。 */
  kind: string;
  /** 单行摘要：解析后的真实路径 / 命令行 / 工具名。 */
  title: string;
  /** 解析后路径（文件类动作）。 */
  path: string | null;
  /** 原始命令（execute 动作）。 */
  command: string | null;
  /** 工作区外标记（文件类动作；null 视为 false）。 */
  outside: boolean;
  /** 「总是允许」的保守 pattern 建议（用户可改后再提交）。 */
  ruleSuggestion: string | null;
}

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
  /** Structured per-tool metadata, set by tool_result / history details.extra. */
  details: Record<string, unknown> | null;
  isError: boolean;
  /**
   * Microcompact 已把该结果替换为占位符（copy-on-write）：卡片折叠为
   * 「已从上下文清除」；clearedCopyMessageId 是副本行 id，「查看原文」
   * 经它回查 original 端点（副本的 supersedes 一跳指向原文）。
   */
  cleared: boolean;
  clearedCopyMessageId: string | null;
  /**
   * 权限门拒绝（details kind=permission_denied）：卡片显示
   * 「已拒绝」徽标。live 路径来自 tool_result 事件的 details，历史路径
   * 来自 DTO details 的已知 kind（与 microcompact_cleared 同一判别位）。
   */
  denied: boolean;
  /** Fork history carries results, never ownership of the source task. */
  inherited?: boolean;
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

/**
 * 上下文压缩标记（compaction marker row）：一段可展开的摘要 + 元数据。
 * stored 路径来自 details kind=compaction 的 user 行；live 路径来自
 * compacted 事件（事件带全量 summary，无需回查）。
 */
export interface CompactionPart {
  kind: "compaction";
  summary: string;
  triggeredBy: "auto" | "manual";
  compactedMessages: number;
  estTokensBefore: number | null;
  estTokensAfter: number | null;
}

/** 后台任务通知行里的单个任务（details.extra.tasks 与文本块 tail 的合并视图）。 */
export interface TaskNotificationTask {
  taskId: string;
  command: string;
  /** BackgroundTaskStatus 同词表（宽松 string，未知原样展示）。 */
  status: string;
  exitCode: number | null;
  error: string | null;
  outputPath: string | null;
  /** 通知内联的输出尾部（可能带 "[earlier output omitted]" 前缀）。 */
  tail: string | null;
}

/**
 * 后台任务通知：任务终态后后端合批插入的 user 行，渲染为
 * 全宽分隔条而非聊天气泡。live 路径没有对应 SSE 事件——行随唤醒 run 的
 * 历史重载/刷新进入视图（stored 路径是唯一来源）。
 */
export interface TaskNotificationPart {
  kind: "task_notification";
  tasks: readonly TaskNotificationTask[];
}

export type DisplayPart =
  | TextPart
  | ToolCallPart
  | ReasoningPart
  | CompactionPart
  | TaskNotificationPart;

export interface DisplayMessage {
  id: string;
  role: "user" | "assistant" | "compaction" | "notification";
  parts: readonly DisplayPart[];
  isStreaming?: boolean;
  forkCut?: ForkCut | null;
}

/** Internal mutable shape used while building/merging messages. */
export interface BuildableMessage {
  id: string;
  role: "user" | "assistant" | "compaction" | "notification";
  parts: DisplayPart[];
  isStreaming?: boolean;
  forkCut?: ForkCut | null;
}

/** Tail cap per tool call for live output kept in the DOM. */
export const LIVE_OUTPUT_MAX_CHARS = 32_768;

// ---------------------------------------------------------------------------
// History replay
// ---------------------------------------------------------------------------

/** Microcompact 占位符文案，与后端 `micro::PLACEHOLDER_CONTENT` 对齐。 */
export const MICROCOMPACT_PLACEHOLDER = "[Old tool result content cleared]";

export function newToolCard(callId: string, name: string | null): ToolCallPart {
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
    cleared: false,
    clearedCopyMessageId: null,
    denied: false,
  };
}

/**
 * Append a live output delta, merging into the trailing segment when the
 * stream is unchanged, then trim past the tail cap (oldest segments first).
 */
export function appendLiveOutput(
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

export function pushText(msg: BuildableMessage, text: string) {
  const last = msg.parts.at(-1);
  if (last?.kind === "text") {
    last.text += text;
  } else {
    msg.parts.push({ kind: "text", text });
  }
}

/**
 * 工具行的透传 metadata（DTO `details.extra`）：已知 kind 之外的键值——
 * 与实时 tool_result 事件的 details 同一形状（标题、截断标记等）。
 */
export function toolExtra(
  details: ApiMessageDetails | null | undefined,
): Record<string, unknown> | null {
  const extra = details?.extra;
  return extra && typeof extra === "object" ? extra : null;
}

/**
 * 标记行的 LLM 可见文本带框架（前缀 + <summary>…</summary> + 后缀）；
 * 分隔条只展示裸摘要。取首个 <summary> 与最后一个 </summary> 之间的
 * 内容，框架缺失时退回全文。
 */
export function summaryFromMarkerText(text: string): string {
  const start = text.indexOf("<summary>");
  const end = text.lastIndexOf("</summary>");
  if (start !== -1 && end > start) {
    return text.slice(start + "<summary>".length, end).trim();
  }
  return text.trim();
}

/**
 * 后台任务通知 DTO details（`extra`）的 kind 判别。dto 层把未识别 kind
 * 原样透传进 `extra`（background_task_notification 不是结构性 kind），
 * 所以历史行读的是 `details.extra.kind` 而非 `details.kind`。
 */
export const TASK_NOTIFICATION_KIND = "background_task_notification";

export function isTaskNotificationRow(
  details: ApiMessageDetails | null | undefined,
): boolean {
  return details?.extra?.kind === TASK_NOTIFICATION_KIND;
}

/** `<task-notification>` 文本块的字段视图（按 "key: value" 行解析）。 */
export interface ParsedNotificationBlock {
  taskId: string;
  command: string;
  status: string;
  /** 退出码原文（数字或 "unknown (killed by a signal)"）。 */
  exitText: string | null;
  outputPath: string | null;
  tail: string;
}

export const TASK_NOTIFICATION_BLOCK_PATTERN =
  /<task-notification>\n([\s\S]*?)\n<\/task-notification>/g;

/**
 * 解析通知 content 里的 `<task-notification>` 块：`output (tail):` 之前的
 * 行按 "key: value" 取字段（task_id/command/status/exit_code/output_file），
 * 之后的内容整段作为输出尾部。
 */
export function parseTaskNotificationBlocks(
  text: string,
): ParsedNotificationBlock[] {
  const blocks: ParsedNotificationBlock[] = [];
  for (const match of text.matchAll(TASK_NOTIFICATION_BLOCK_PATTERN)) {
    const lines = (match[1] ?? "").split("\n");
    const fields: Record<string, string> = {};
    let tail = "";
    for (let i = 0; i < lines.length; i++) {
      const line = lines[i]!;
      if (line.startsWith("output (tail):")) {
        tail = lines.slice(i + 1).join("\n");
        break;
      }
      const sep = line.indexOf(": ");
      if (sep > 0) fields[line.slice(0, sep)] = line.slice(sep + 2);
    }
    if (!fields.task_id) continue;
    blocks.push({
      taskId: fields.task_id,
      command: fields.command ?? "",
      status: fields.status ?? "",
      exitText: fields.exit_code ?? null,
      outputPath: fields.output_file ?? null,
      tail,
    });
  }
  return blocks;
}

export function toNotificationTask(
  raw: Record<string, unknown>,
  tails: Map<string, string>,
): TaskNotificationTask {
  const num = (value: unknown): number | null =>
    typeof value === "number" ? value : null;
  const str = (value: unknown): string | null =>
    typeof value === "string" ? value : null;
  const taskId = str(raw.task_id) ?? "";
  return {
    taskId,
    command: str(raw.command) ?? "",
    status: str(raw.status) ?? "",
    exitCode: num(raw.exit_code),
    error: str(raw.error),
    outputPath: str(raw.output_path),
    tail: tails.get(taskId) ?? null,
  };
}

export function blockToNotificationTask(
  block: ParsedNotificationBlock,
): TaskNotificationTask {
  return {
    taskId: block.taskId,
    command: block.command,
    status: block.status,
    exitCode: Number.isFinite(Number(block.exitText))
      ? Number(block.exitText)
      : null,
    error: null,
    outputPath: block.outputPath,
    tail: block.tail || null,
  };
}

/**
 * Build display messages from the typed history DTO. Tool messages are not
 * rendered as bubbles: their tool_result parts fill the matching tool card
 * (which may live in an earlier assistant message), re-applying the row's
 * passthrough metadata so cards re-render identically to the live stream.
 * Orphan results (old data without a matching card) become a minimal done
 * card named "unknown".
 */
export function storedToDisplayList(stored: ApiMessage[]): DisplayMessage[] {
  const out: BuildableMessage[] = [];
  const cardsByCallId = new Map<string, ToolCallPart>();
  let lastAssistant: BuildableMessage | null = null;

  const fillCard = (
    card: ToolCallPart,
    result: Extract<ApiMessagePart, { type: "tool_result" }>,
    details: ApiMessageDetails | null | undefined,
    rowId: string,
  ) => {
    card.status = "done";
    card.content =
      typeof result.content === "string"
        ? result.content
        : JSON.stringify(result.content, null, 2);
    card.isError = result.is_error;
    const extra = toolExtra(details);
    if (!card.title && typeof extra?.title === "string") {
      card.title = extra.title;
    }
    card.details = extra;
    // Microcompact 副本行（details kind 标记）：卡片折叠为已清除态；行 id
    // 即副本 id，供「查看原文」回查。
    if (details?.kind === "microcompact_cleared") {
      card.cleared = true;
      card.clearedCopyMessageId = rowId;
    }
    // 权限门拒绝行（details kind=permission_denied）：「已拒绝」徽标。
    if (details?.kind === "permission_denied") {
      card.denied = true;
    }
  };

  for (const m of stored) {
    if (m.role === "user") {
      // 压缩标记行（details kind=compaction）不渲染成气泡，而是独立的
      // 全宽分隔条（可展开摘要）。
      if (m.details?.kind === "compaction") {
        let text = "";
        for (const part of m.parts) {
          if (part.type === "text") text += part.text;
        }
        const compaction = m.details.compaction;
        out.push({
          id: m.id,
          role: "compaction",
          parts: [
            {
              kind: "compaction",
              summary: summaryFromMarkerText(text),
              triggeredBy:
                compaction?.triggered_by === "manual" ? "manual" : "auto",
              compactedMessages: compaction?.compacted_message_count ?? 0,
              estTokensBefore: compaction?.estimated_tokens_before ?? null,
              estTokensAfter: compaction?.estimated_tokens_after ?? null,
            },
          ],
        });
        lastAssistant = null;
        continue;
      }
      // 后台任务通知行（details.extra.kind=background_task_notification，
      // 后端在任务终态后插入）：全宽分隔条（可展开尾部/定位卡片），渲染为
      // 独立角色而非聊天气泡。结构化字段来自 extra.tasks，输出尾部从
      // content 的 <task-notification> 文本块解析。
      if (isTaskNotificationRow(m.details)) {
        let text = "";
        for (const part of m.parts) {
          if (part.type === "text") text += part.text;
        }
        const parsed = parseTaskNotificationBlocks(text);
        const tails = new Map(
          parsed.map((block) => [block.taskId, block.tail]),
        );
        const rawTasks = m.details?.extra?.tasks;
        const rows = Array.isArray(rawTasks)
          ? rawTasks
              .filter(
                (raw): raw is Record<string, unknown> =>
                  raw !== null && typeof raw === "object",
              )
              .map((raw) => toNotificationTask(raw, tails))
          : [];
        out.push({
          id: m.id,
          role: "notification",
          parts: [
            {
              kind: "task_notification",
              // details.tasks 缺失（异常行）时回退纯文本解析，保证可见。
              tasks:
                rows.length > 0
                  ? rows
                  : parsed.map((block) => blockToNotificationTask(block)),
            },
          ],
        });
        lastAssistant = null;
        continue;
      }
      const msg: BuildableMessage = {
        id: m.id,
        role: "user",
        parts: [],
        forkCut: m.fork_cut,
      };
      for (const part of m.parts) {
        if (part.type === "text") pushText(msg, part.text);
      }
      out.push(msg);
      lastAssistant = null;
      continue;
    }

    if (m.role === "assistant") {
      const msg: BuildableMessage = {
        id: m.id,
        role: "assistant",
        parts: [],
        forkCut: m.fork_cut,
      };
      // Thinking duration is UI metadata (DTO details), not a message part.
      const storedDurationMs = m.details?.reasoning_duration_ms ?? null;
      for (const part of m.parts) {
        if (part.type === "text") {
          pushText(msg, part.text);
        } else if (part.type === "reasoning") {
          msg.parts.push({
            kind: "reasoning",
            text: part.text,
            startedAt: null,
            durationMs: storedDurationMs,
            ended: true,
          });
        } else if (part.type === "tool_call") {
          const card = newToolCard(part.id, part.name);
          card.inherited = m.inherited ?? false;
          card.arguments = part.arguments ?? "";
          // Stored calls have finished executing; the matching tool message
          // (if any) fills in content/details below.
          card.status = "done";
          msg.parts.push(card);
          cardsByCallId.set(card.callId, card);
        }
        // image_url parts are not displayed (no UI for them).
      }
      out.push(msg);
      lastAssistant = msg;
      continue;
    }

    if (m.role === "tool") {
      for (const part of m.parts) {
        if (part.type !== "tool_result") continue;
        const card = cardsByCallId.get(part.call_id);
        if (card) {
          fillCard(card, part, m.details, m.id);
        } else {
          // Orphan result (e.g. data from before cards were stored): host a
          // minimal done card on the most recent assistant message.
          if (!lastAssistant) {
            lastAssistant = { id: m.id, role: "assistant", parts: [] };
            out.push(lastAssistant);
          }
          const orphan = newToolCard(part.call_id, "unknown");
          orphan.inherited = m.inherited ?? false;
          orphan.status = "done";
          lastAssistant.parts.push(orphan);
          fillCard(orphan, part, m.details, m.id);
        }
      }
      // A server-authorized tool cut includes the whole closed tool batch.
      // Keep its persisted result id even though results render on the assistant.
      if (m.fork_cut && lastAssistant) lastAssistant.forkCut = m.fork_cut;
    }
  }

  // 悬空调用：始终没等到配对 ToolResult（fillCard 未被调用，content 仍为
  // null）的卡片标注为已中断——后端崩溃留下的历史，恢复前 UI 的可见信号。
  for (const card of cardsByCallId.values()) {
    if (card.content === null) card.status = "interrupted";
  }

  return out;
}

/** 历史里最近一次带 usage 的 assistant 行的 input_tokens（无则 null）。 */
export function lastAssistantInputTokens(
  stored: readonly ApiMessage[],
): number | null {
  for (let i = stored.length - 1; i >= 0; i--) {
    const m = stored[i];
    if (m.role === "assistant" && m.input_tokens != null) {
      return m.input_tokens;
    }
  }
  return null;
}
