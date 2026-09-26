import { computed, readonly, ref } from "vue";
import client from "@/api/client";
import {
  clearContextUsage,
  recordUsageEvent,
  seedContextUsage,
  setCompacting,
} from "@/composables/useContextUsage";
import { applySubagentEvent } from "@/composables/useSubagents";
import type { components } from "@/api/schema";
import type { SubagentUpdatedEventData } from "@/composables/useSubagents";

type ApiMessage = components["schemas"]["MessageDto"];
type ApiMessagePart = components["schemas"]["MessagePart"];
type ApiMessageDetails = components["schemas"]["MessageDetails"];
type AgentEvent = components["schemas"]["AgentEvent"];
type OutputStream = components["schemas"]["OutputStream"];
type PendingApprovalDto = components["schemas"]["PendingApprovalDto"];

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
}

/** Internal mutable shape used while building/merging messages. */
interface BuildableMessage {
  id: string;
  role: "user" | "assistant" | "compaction" | "notification";
  parts: DisplayPart[];
  isStreaming?: boolean;
}

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

/** Tail cap per tool call for live output kept in the DOM. */
const LIVE_OUTPUT_MAX_CHARS = 32_768;

// ---------------------------------------------------------------------------
// History replay
// ---------------------------------------------------------------------------

/** Microcompact 占位符文案，与后端 `micro::PLACEHOLDER_CONTENT` 对齐。 */
export const MICROCOMPACT_PLACEHOLDER = "[Old tool result content cleared]";

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
    cleared: false,
    clearedCopyMessageId: null,
    denied: false,
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

/**
 * 工具行的透传 metadata（DTO `details.extra`）：已知 kind 之外的键值——
 * 与实时 tool_result 事件的 details 同一形状（标题、截断标记等）。
 */
function toolExtra(
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
function summaryFromMarkerText(text: string): string {
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
const TASK_NOTIFICATION_KIND = "background_task_notification";

function isTaskNotificationRow(
  details: ApiMessageDetails | null | undefined,
): boolean {
  return details?.extra?.kind === TASK_NOTIFICATION_KIND;
}

/** `<task-notification>` 文本块的字段视图（按 "key: value" 行解析）。 */
interface ParsedNotificationBlock {
  taskId: string;
  command: string;
  status: string;
  /** 退出码原文（数字或 "unknown (killed by a signal)"）。 */
  exitText: string | null;
  outputPath: string | null;
  tail: string;
}

const TASK_NOTIFICATION_BLOCK_PATTERN =
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

function toNotificationTask(
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

function blockToNotificationTask(
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
function storedToDisplayList(stored: ApiMessage[]): DisplayMessage[] {
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
      const msg: BuildableMessage = { id: m.id, role: "user", parts: [] };
      for (const part of m.parts) {
        if (part.type === "text") pushText(msg, part.text);
      }
      out.push(msg);
      lastAssistant = null;
      continue;
    }

    if (m.role === "assistant") {
      const msg: BuildableMessage = { id: m.id, role: "assistant", parts: [] };
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
          orphan.status = "done";
          lastAssistant.parts.push(orphan);
          fillCard(orphan, part, m.details, m.id);
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

/** 历史里最近一次带 usage 的 assistant 行的 input_tokens（无则 null）。 */
function lastAssistantInputTokens(
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

// ---------------------------------------------------------------------------
// 发送失败文案（sendMessage 的 !res.ok 分支）
// ---------------------------------------------------------------------------

/** pending_recovery 引导文案（后端 409 code=pending_recovery）。 */
const PENDING_RECOVERY_HINT =
  "存在未完成的子代理任务，请使用「恢复此轮」继续或停止旧任务";

/**
 * 归一化 send 失败的可展示文本：错误体带 code=pending_recovery 时附引导
 * 文案（根会话有未结算的 crashed 子任务）；其余失败沿用既有提示（历史
 * 测试与 UI 均断言 "Failed to send message."）。非 JSON 错误体静默降级。
 */
export async function sendFailureText(res: Response): Promise<string> {
  try {
    const body = (await res.json()) as {
      code?: unknown;
      error?: unknown;
    };
    if (body && typeof body === "object" && body.code === "pending_recovery") {
      const message =
        typeof body.error === "string" && body.error
          ? body.error
          : `HTTP ${res.status}`;
      return `Failed to send message.\n\n${message}\n${PENDING_RECOVERY_HINT}`;
    }
  } catch {
    // 非 JSON 错误体（如裸文本）：走通用文案
  }
  return "Failed to send message.";
}

// ---------------------------------------------------------------------------
// 可实例化的聊天会话（createChatSession）
// ---------------------------------------------------------------------------

export interface CreateChatSessionOptions {
  /**
   * 是否把 usage / 压缩状态写进全局 useContextUsage 单例（主面板输入框
   * 圆环的数据源）。子代理面板必须传 false——子 Finish/Usage 事件不得
   * 结束父流或覆盖父上下文环；实例自身的 usage 快照（返回值
   * 的 usage 字段）始终记录，供面板本地的用量环消费。默认 true。
   */
  contextUsage?: boolean;
  /**
   * `subagent_updated` 事件派发（SSE 消费层拦截，携带流所属会话 id 与
   * 负载）。仅主面板单例接线到 useSubagents 任务 store；子代理面板的
   * 实例不接线（子 hub 不发该事件，隔离由构造保证）。
   */
  onSubagentEvent?: (
    conversationId: string,
    data: SubagentUpdatedEventData,
  ) => void;
}

interface StreamState {
  messageId: string;
  parts: DisplayPart[];
  isStreaming: boolean;
}

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

const FLUSH_INTERVAL_MS = 50;

/**
 * 创建一个独立的聊天会话实例：消息/流式/usage 状态都在实例闭包内，
 * 父（useChat 单例）与子代理 panel 各持一份，互不污染。SSE 解析与
 * reducer 共用同一套实现（闭包内函数），历史转换等纯函数保持模块级。
 */
export function createChatSession(options: CreateChatSessionOptions = {}) {
  const trackContextUsage = options.contextUsage !== false;

  const messages = ref<DisplayMessage[]>([]);
  const sending = ref(false);

  /**
   * Dedupe guard: the route watch in [id].vue and the tab-return refresh in
   * App.vue can fire for the same conversation within one tick (sidebar
   * switch while a non-chat tab is active); collapse repeats into one request.
   */
  let lastFetch: { id: string; at: number } | null = null;

  /**
   * `messages` 当前展示的会话 id（null = 空态）。流式收尾的静默刷新以它
   * 判断视图是否仍在该会话上：resume/send 进行中用户可能已切走，无条件
   * 刷新会用旧会话的历史覆盖当前视图。子代理面板里它是 agent id
   * （子会话 id），作用域语义一致。
   */
  let activeConversationId: string | null = null;

  /** 实例本地的上下文占用快照（usage 事件 + 历史播种）。 */
  const usageInputTokens = ref<number | null>(null);
  const usageWindowTokens = ref<number | null>(null);
  const usageCompacting = ref(false);

  // --- 审批状态 ---------------------------------------------------------------
  // 按会话分桶的 pending 审批：事件（upsert/remove）与 pending 快照端点
  // （整表替换）两路维护；重放幂等由「requested 后必跟 resolved」的事件
  // 时序保证。审批内存即焚，不进消息 reducer。

  /** 会话 id → 待决审批列表（审批卡的数据源）。 */
  const pendingApprovals = ref(new Map<string, PendingApproval[]>());

  /** approval_requested 事件负载 → 展示形状。 */
  function approvalFromEvent(
    data: Extract<AgentEvent, { type: "approval_requested" }>["data"],
  ): PendingApproval {
    return {
      approvalId: data.approval_id,
      callId: data.call_id,
      tool: data.tool,
      kind: data.kind,
      title: data.title,
      path: data.path ?? null,
      command: data.command ?? null,
      outside: data.outside === true,
      ruleSuggestion: data.rule_suggestion ?? null,
    };
  }

  /** pending 快照 DTO → 展示形状（与事件负载同字段，另带 run/created_at）。 */
  function approvalFromDto(dto: PendingApprovalDto): PendingApproval {
    return {
      approvalId: dto.approval_id,
      callId: dto.call_id,
      tool: dto.tool,
      kind: dto.kind,
      title: dto.title,
      path: dto.path ?? null,
      command: dto.command ?? null,
      outside: dto.outside === true,
      ruleSuggestion: dto.rule_suggestion ?? null,
    };
  }

  /** 按 approvalId upsert（重放重复到达时原地覆盖，不产生重复卡片）。 */
  function upsertApproval(conversationId: string, approval: PendingApproval) {
    const list = pendingApprovals.value.get(conversationId) ?? [];
    const index = list.findIndex(
      (item) => item.approvalId === approval.approvalId,
    );
    const next =
      index === -1 ? [...list, approval] : list.with(index, approval);
    pendingApprovals.value = new Map(pendingApprovals.value).set(
      conversationId,
      next,
    );
  }

  /** 移除一条 pending（resolved 事件 / decide 落地；未命中时静默幂等）。 */
  function removeApproval(conversationId: string, approvalId: string) {
    const list = pendingApprovals.value.get(conversationId);
    if (!list?.some((item) => item.approvalId === approvalId)) return;
    pendingApprovals.value = new Map(pendingApprovals.value).set(
      conversationId,
      list.filter((item) => item.approvalId !== approvalId),
    );
  }

  /**
   * 拉取会话的 pending 快照并以 GET 为准整表替换该会话的分桶（页面刷新/
   * 重连后的真相源；事件重放在其之后叠加）。尽力而为：失败（网络/非 2xx）
   * 保留旧列表静默返回，绝不阻断 attachRun 的重连主流程。
   */
  const refreshApprovals = async (conversationId: string) => {
    try {
      const { data, error } = await client.GET(
        "/api/conversations/{id}/approvals",
        { params: { path: { id: conversationId } } },
      );
      if (!error && data) {
        pendingApprovals.value = new Map(pendingApprovals.value).set(
          conversationId,
          data.map(approvalFromDto),
        );
      }
    } catch {
      // 网络层异常：与 HTTP 错误同路径，保留旧列表
    }
  };

  /**
   * 应答一条审批：POST decide；成功（或 404=已决/未知）即本地清掉
   * 条目——approval_resolved 事件随后到达时的移除是幂等空操作。其余错误
   * 保留卡片供重试。
   */
  const decideApproval = async (
    conversationId: string,
    approvalId: string,
    decision: ApprovalDecision,
    pattern?: string,
  ) => {
    const { error, response } = await client.POST(
      "/api/conversations/{id}/approvals/{approval_id}",
      {
        params: { path: { id: conversationId, approval_id: approvalId } },
        body: { decision, pattern: pattern ?? null },
      },
    );
    // 404 = 已决/未知 id（decide 的幂等语义）：与成功同等对待，静默清卡。
    const settled = !error || response.status === 404;
    if (settled) removeApproval(conversationId, approvalId);
    return settled;
  };

  /** Plain (non-reactive) mirror of the streaming message; flushed every tick. */
  let streamState: StreamState | null = null;
  let flushTimer: ReturnType<typeof setInterval> | null = null;
  /** Set by applyStreamEvent; avoids re-rendering unchanged state every tick. */
  let streamDirty = false;

  /** 子代理 events 订阅的中止器（dispose 时中断在飞订阅）。 */
  let attachAbort: AbortController | null = null;
  /** dispose 后不再消费/落位任何事件（abort 传播到 read 拒绝前的缓冲事件兜底）。 */
  let disposed = false;

  // -------------------------------------------------------------------------
  // Live streaming (non-reactive buffers flushed into `messages` on a timer)
  // -------------------------------------------------------------------------

  /** Write the buffered stream state into the reactive messages array. */
  function flushStream() {
    if (!streamState || !streamDirty) return;
    streamDirty = false;
    const idx = messages.value.findIndex(
      (m) => m.id === streamState?.messageId,
    );
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

  function recordUsage(data: {
    input_tokens?: number | null;
    context_window_tokens?: number | null;
  }) {
    if (data.input_tokens != null) usageInputTokens.value = data.input_tokens;
    if (data.context_window_tokens != null) {
      usageWindowTokens.value = data.context_window_tokens;
    }
    if (trackContextUsage) recordUsageEvent(data);
  }

  function applyStreamEvent(event: ChatStreamEvent) {
    if (event.type === "subagent_updated") {
      // 正常路径在 consumeSseStream 拦截派发（携带流所属会话 id）；该事件
      // 属会话级任务状态而非消息内容，不进消息 reducer。
      return;
    }
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
          // 历史消息的卡片里——必须遍历 messages.value 更新（经代理路径改，
          // 保证响应性）。当前流式消息那份只在 streamState 里改：它在
          // messages.value 中的克隆会被 flushStream 整体覆盖。
          const live = findCard(r.call_id);
          if (live) applyCleared(live, r.copy_message_id);
          for (let i = messages.value.length - 1; i >= 0; i--) {
            const m = messages.value[i]!;
            if (streamState && m.id === streamState.messageId) continue;
            for (const p of m.parts) {
              if (p.kind === "tool_call" && p.callId === r.call_id) {
                applyCleared(p, r.copy_message_id);
              }
            }
          }
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

      case "compacting": {
        // 摘要生成中：状态栏指示器切换为「压缩中」，不进消息流。
        usageCompacting.value = true;
        if (trackContextUsage) setCompacting(true);
        break;
      }

      case "compacted": {
        // 事件带全量 summary：当场插入分隔条（此刻通常位于本轮流式消息的
        // 最前部），run 收尾的 loadMessages 会用 stored 标记行接管同一展示。
        usageCompacting.value = false;
        if (trackContextUsage) setCompacting(false);
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
        recordUsage(event.data);
        break;
      }
    }
  }

  // -------------------------------------------------------------------------
  // SSE 消费（sendMessage / resumeTurn / attach 共用）
  // -------------------------------------------------------------------------

  /**
   * 读取一个 SSE Response 直到流 EOF，把每个 data: 事件交给回调（默认
   * applyStreamEvent；attachRun 借此拦截 run_meta）。title_updated 是会话
   * 元数据更新，在此直接派发给监听器、不进回调；subagent_updated 同为
   * 会话作用域的小状态事件（任务 store 的数据源），拦截后经
   * options.onSubagentEvent 派发（携带流所属会话 id）。网络分片可能在
   * 任意字节边界断开（含 data: 前缀与 JSON 中间），逐行缓冲解析。
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

        let event: ChatStreamEvent;
        try {
          event = JSON.parse(payload) as ChatStreamEvent;
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
        if (event.type === "subagent_updated") {
          options.onSubagentEvent?.(conversationId, event.data);
          continue;
        }
        // 审批事件是会话作用域状态而非消息内容：在此按流所属会话 upsert/
        // remove（不依赖 activeConversationId），也不进消息 reducer。
        if (event.type === "approval_requested") {
          upsertApproval(conversationId, approvalFromEvent(event.data));
          continue;
        }
        if (event.type === "approval_resolved") {
          removeApproval(conversationId, event.data.approval_id);
          continue;
        }
        onEvent(event);
      }
    }
  }

  // -------------------------------------------------------------------------

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
      usageInputTokens.value = lastAssistantInputTokens(data);
      if (trackContextUsage) seedContextUsage(data);
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

    // 非 2xx 时从错误体提取可展示文案（pending_recovery 附引导提示）；
    // 网络层异常仍走通用 fallback。
    let failureText: string | null = null;
    try {
      const body: { content: string; provider_spec?: string } = { content };
      if (providerSpec) body.provider_spec = providerSpec;
      const res = await fetch(`/api/conversations/${conversationId}/messages`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      });

      if (!res.ok || !res.body) {
        failureText = await sendFailureText(res);
        throw new Error(`HTTP ${res.status}`);
      }

      await consumeSseStream(res, conversationId);
    } catch {
      if (streamState) {
        streamState.parts.push({
          kind: "text",
          text: failureText ?? "Failed to send message.",
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

      // 先拉一次 pending 审批快照（页面刷新后的真相源），
      // 再订阅事件流——重放的 requested/resolved 事件在其后叠加，最终
      // 状态与后端注册表一致。
      await refreshApprovals(conversationId);

      const res = await fetch(`/api/conversations/${conversationId}/events`);
      if (!res.ok || !res.body) {
        // 404：探测与订阅之间 run 结束——回到 DB 历史（finally 刷新）。
        throw new Error(`HTTP ${res.status}`);
      }

      // 截断锚点：run_meta.start_message_id 对应的 user 消息行；后端没记
      // 录（如 resume 续跑的 run，无新 user 消息）时兜底锚到最后一条 user
      // 消息。后台任务唤醒 run 的锚点是通知行（role=notification，等价
      // user 轮尾）。截断必须在应用任何 delta 之前完成，因此放在注入回调
      // 里、随首个事件执行恰好一次。
      let truncated = false;
      const lastUserIndex = () => {
        for (let i = messages.value.length - 1; i >= 0; i--) {
          const role = messages.value[i]?.role;
          if (role === "user" || role === "notification") return i;
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
   * 子代理面板的挂载入口：拉取子会话历史（/api/subagents/{agent_id}/
   * messages，MessageDto 同形），再探测其当前 run 的事件流（/events）。
   * 404 = 无活动 run，仅展示历史；200 则与 attachRun 同一套 run_meta
   * 锚点截断 + 事件重放 + 实时增长语义（SSE 解析/reducer 完全共用）。
   * usage 只进实例本地快照（不污染主面板的全局上下文环）。
   */
  const attachSubagent = async (agentId: string) => {
    if (sending.value) return;
    sending.value = true;
    disposed = false;
    attachAbort?.abort();
    const controller = new AbortController();
    attachAbort = controller;

    /** 拉取子会话历史并落位（不经过 fetchMessages 的会话级守卫）。 */
    const loadSubagentHistory = async () => {
      const { data, error } = await client.GET(
        "/api/subagents/{agentId}/messages",
        { params: { path: { agentId } } },
      );
      if (!error && data) {
        messages.value = storedToDisplayList(data);
        activeConversationId = agentId;
        usageInputTokens.value = lastAssistantInputTokens(data);
        usageWindowTokens.value = null;
      }
    };

    try {
      await loadSubagentHistory();

      const res = await fetch(`/api/subagents/${agentId}/events`, {
        signal: controller.signal,
      });
      if (res.status === 404) {
        // 无活动 run（已结束）：仅历史视图。
        return;
      }
      if (!res.ok || !res.body) {
        throw new Error(`HTTP ${res.status}`);
      }

      // 与 attachRun 相同的截断锚点逻辑（run_meta 合成事件只在
      // /events 重放里注入）。
      let truncated = false;
      const lastUserIndex = () => {
        for (let i = messages.value.length - 1; i >= 0; i--) {
          if (messages.value[i]?.role === "user") return i;
        }
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

      await consumeSseStream(res, agentId, (event) => {
        // 会话守卫 + dispose 守卫：关闭标签后（或切走后）本流的事件不再进
        // 视图——abort 传播到 read 拒绝前仍可能有已缓冲的事件到达。
        if (disposed || activeConversationId !== agentId) return;
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
      // 网络失败/主动中断：静默，finally 统一收敛（中断时跳过收尾刷新）。
    } finally {
      if (attachAbort === controller) attachAbort = null;
      stopFlushing();
      if (streamState) {
        finalizeUnconvergedCards();
        streamState.isStreaming = false;
        flushStream();
        streamState = null;
      }
      if (!controller.signal.aborted && activeConversationId === agentId) {
        try {
          await loadSubagentHistory();
        } catch {
          // ignore: cosmetic refresh
        }
      }
      sending.value = false;
    }
  };

  /**
   * 释放实例持有的流订阅（关闭子代理标签时调用）：中断在飞的 events
   * 订阅。只是退订——后端执行不受影响，关闭子标签不取消任务。
   */
  const dispose = () => {
    disposed = true;
    attachAbort?.abort();
    attachAbort = null;
    stopFlushing();
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

  /**
   * 手动压缩（/compact）：同步请求，成功后重拉历史（stored 标记行接管
   * 分隔条展示）。失败静默（console 记录）——无 toast 基建，v1 以「分隔
   * 条未出现」为可见信号。
   */
  const compactConversation = async (conversationId: string) => {
    if (sending.value) return;
    sending.value = true;
    usageCompacting.value = true;
    if (trackContextUsage) setCompacting(true);
    try {
      const { data, error } = await client.POST(
        "/api/conversations/{id}/compact",
        { params: { path: { id: conversationId } } },
      );
      if (error || !data) {
        console.error("compact failed", error ?? "no data");
        return;
      }
      if (activeConversationId === conversationId) {
        await loadMessages(conversationId);
      }
    } catch (error) {
      console.error("compact failed", error);
    } finally {
      usageCompacting.value = false;
      if (trackContextUsage) setCompacting(false);
      sending.value = false;
    }
  };

  const clearMessages = () => {
    messages.value = [];
    activeConversationId = null;
    usageInputTokens.value = null;
    usageWindowTokens.value = null;
    usageCompacting.value = false;
    pendingApprovals.value = new Map();
    if (trackContextUsage) clearContextUsage();
  };

  /** 存在被中断的轮次可恢复：悬空工具卡片，或最后一条消息是 user
   *（含后台任务通知行——后端把它作为 user 行插入，唤醒 run 启动失败时
   * 它就是可续跑的轮尾）。 */
  const resumable = computed(() => {
    if (messages.value.length === 0) return false;
    const lastRole = messages.value.at(-1)?.role;
    if (lastRole === "user" || lastRole === "notification") {
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
    usage: {
      inputTokens: readonly(usageInputTokens),
      windowTokens: readonly(usageWindowTokens),
      compacting: readonly(usageCompacting),
    },
    /** 会话 id → 待决审批列表（ApprovalBanner 的数据源，按当前会话取桶）。 */
    pendingApprovals: readonly(pendingApprovals),
    fetchMessages,
    sendMessage,
    resumeTurn,
    attachRun,
    attachSubagent,
    cancelConversation,
    compactConversation,
    decideApproval,
    refreshApprovals,
    clearMessages,
    dispose,
  };
}

/**
 * 主面板的模块级单会话实例：`subagent_updated` 事件在此接线到
 * useSubagents 任务 store（携带流所属会话 id，store 侧做迟到/跨会话
 * 隔离）。子代理面板一律经 createChatSession 另建实例，不碰单例。
 */
const singletonSession = createChatSession({
  onSubagentEvent: (conversationId, data) => {
    applySubagentEvent(conversationId, data);
  },
});

export function useChat() {
  return singletonSession;
}
