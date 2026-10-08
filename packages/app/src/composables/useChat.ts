import { computed, readonly, ref } from "vue";
import client from "@/api/client";
import {
  clearContextUsage,
  recordUsageEvent,
  seedContextUsage,
  setCompacting,
} from "@/composables/useContextUsage";
import { applySubagentEvent } from "@/composables/useSubagents";
import {
  appendLiveOutput,
  lastAssistantInputTokens,
  MICROCOMPACT_PLACEHOLDER,
  newToolCard,
  storedToDisplayList,
} from "./chat/display";
import { createRunController } from "./chat/run";
import { readChatStream } from "./chat/transport";
import type {
  AgentEvent,
  ApprovalDecision,
  ChatStreamEvent,
  DisplayMessage,
  DisplayPart,
  PendingApproval,
  PendingApprovalDto,
  ToolCallPart,
} from "./chat/display";
import type { RunOwner } from "./chat/run";
import type { SubagentUpdatedEventData } from "@/composables/useSubagents";

export * from "./chat/display";

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
    if (body?.code === "stale_revision")
      return "会话历史已变化，请刷新后重新发送。";
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
const CONTEXT_REVISION_PATTERN = /^\d+$/;

/**
 * 创建一个独立的聊天会话实例：消息/流式/usage 状态都在实例闭包内，
 * 父（useChat 单例）与子代理 panel 各持一份，互不污染。SSE 解析与
 * reducer 共用同一套实现（闭包内函数），历史转换等纯函数保持模块级。
 */
export function createChatSession(options: CreateChatSessionOptions = {}) {
  const trackContextUsage = options.contextUsage !== false;

  const messages = ref<DisplayMessage[]>([]);
  const runs = createRunController();
  const sending = runs.busy;
  let historyChanging = false;
  const serverResumable = ref(false);

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
  let historyGeneration = 0;

  /** 实例本地的上下文占用快照（usage 事件 + 历史播种）。 */
  const usageInputTokens = ref<number | null>(null);
  const contextRevision = ref<number | null>(null);
  // Failed sends invalidate history actions while keeping their temporary
  // output visible. Retain the snapshot revision separately to guard retries.
  let sendRevision: number | null = null;
  const usageWindowTokens = ref<number | null>(null);
  const usageCompacting = ref(false);

  /** dispose 后不再消费/落位任何事件（abort 传播到 read 拒绝前的缓冲事件兜底）。 */
  let disposed = false;

  // --- 审批状态 ---------------------------------------------------------------
  // 按会话分桶的 pending 审批：事件（upsert/remove）与 pending 快照端点
  // （整表替换）两路维护；重放幂等由「requested 后必跟 resolved」的事件
  // 时序保证。审批内存即焚，不进消息 reducer。

  /** 会话 id → 待决审批列表（审批卡的数据源）。 */
  const pendingApprovals = ref(new Map<string, PendingApproval[]>());
  const approvalVersions = new Map<string, number>();
  let approvalSnapshotGeneration = 0;
  const approvalLoads = new Map<string, number>();

  /** approval_requested 事件负载 → 展示形状。 */
  function approvalFromEvent(
    data: Extract<AgentEvent, { type: "approval_requested" }>["data"],
  ): PendingApproval {
    return {
      approvalId: data.approval_id,
      agentId: data.agent_id ?? null,
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
      agentId: dto.agent_id ?? null,
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
    approvalVersions.set(
      conversationId,
      (approvalVersions.get(conversationId) ?? 0) + 1,
    );
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
    approvalVersions.set(
      conversationId,
      (approvalVersions.get(conversationId) ?? 0) + 1,
    );
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
    const generation = ++approvalSnapshotGeneration;
    const version = approvalVersions.get(conversationId) ?? 0;
    approvalLoads.set(conversationId, generation);
    try {
      const { data, error } = await client.GET(
        "/api/conversations/{id}/approvals",
        { params: { path: { id: conversationId } } },
      );
      if (
        !disposed &&
        !error &&
        data &&
        approvalLoads.get(conversationId) === generation &&
        (approvalVersions.get(conversationId) ?? 0) === version
      ) {
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

  function selectConversation(conversationId: string | null) {
    if (runs.select(conversationId)) {
      ++historyGeneration;
      messages.value = [];
      serverResumable.value = false;
      sendRevision = null;
      contextRevision.value = null;
      stopFlushing();
      streamState = null;
      streamDirty = false;
      usageInputTokens.value = null;
      usageWindowTokens.value = null;
      usageCompacting.value = false;
      if (trackContextUsage) clearContextUsage();
    }
    activeConversationId = conversationId;
  }

  function beginRun(conversationId: string): RunOwner | null {
    if (historyChanging || disposed) return null;
    selectConversation(conversationId);
    return runs.begin(conversationId);
  }

  async function finalizeRun(
    owner: RunOwner,
    refresh?: () => Promise<void>,
    reconnect?: () => Promise<void>,
  ) {
    if (!runs.owns(owner)) return;
    stopFlushing();
    usageCompacting.value = false;
    if (trackContextUsage) setCompacting(false);
    if (streamState) {
      finalizeUnconvergedCards();
      streamState.isStreaming = false;
      streamDirty = true;
      flushStream();
      streamState = null;
    }
    if (refresh) {
      try {
        await refresh();
      } catch {
        /* persisted history converges on the next load */
      }
    }
    if (!runs.finish(owner)) return;
    if (reconnect) await reconnect();
  }

  function showRunFailure(owner: RunOwner, text: string) {
    if (!runs.owns(owner)) return;
    if (streamState) {
      if (
        !messages.value.some((message) => message.id === streamState?.messageId)
      )
        return;
      streamState.parts.push({ kind: "text", text });
      streamDirty = true;
    } else {
      messages.value = [
        ...messages.value,
        {
          id: `error-${owner.generation}`,
          role: "assistant",
          parts: [{ kind: "text", text }],
        },
      ];
    }
    contextRevision.value = null;
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

      case "nested_tool_update": {
        const card = findCard(event.data.parent_call_id);
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
    owner: RunOwner,
    onEvent: (event: AgentEvent) => void = applyStreamEvent,
  ): Promise<boolean> {
    const conversationId = owner.conversationId;
    let failed = false;
    const terminal = await readChatStream(
      res,
      (event) => {
        if (!runs.owns(owner)) return;
        if (event.type === "error" || event.type === "cancelled") failed = true;
        if (
          event.type === "finish" &&
          activeConversationId === conversationId &&
          !disposed
        ) {
          serverResumable.value = false;
        }
        if (event.type === "title_updated") {
          // 派发放在 parse 的 try 之外：监听器异常不该被当成坏帧吞掉，
          // 也不该中断其余监听器收到本事件。
          for (const listener of titleListeners) {
            try {
              listener(conversationId, event.data.title);
            } catch {
              /* isolate listeners */
            }
          }
          return;
        }
        if (event.type === "subagent_updated") {
          options.onSubagentEvent?.(conversationId, event.data);
          return;
        }
        // 审批事件是会话作用域状态而非消息内容：在此按流所属会话 upsert/
        // remove（不依赖 activeConversationId），也不进消息 reducer。
        if (event.type === "approval_requested") {
          upsertApproval(conversationId, approvalFromEvent(event.data));
          return;
        }
        if (event.type === "approval_resolved") {
          removeApproval(conversationId, event.data.approval_id);
          return;
        }
        if (!disposed && activeConversationId === conversationId)
          onEvent(event);
      },
      owner.signal,
    );
    if (failed && runs.owns(owner)) {
      try {
        const { data } = await client.GET("/api/conversations/{id}/run", {
          params: { path: { id: conversationId } },
        });
        if (runs.owns(owner)) {
          serverResumable.value = data?.resumable ?? false;
        }
      } catch {
        /* status will be retried on reattachment */
      }
    }
    return terminal;
  }

  // -------------------------------------------------------------------------

  /** 实际拉取并重建 messages（绕过 fetchMessages 的 200ms 去重守卫）。 */
  const loadMessages = async (
    conversationId: string,
    requireRevision = false,
  ) => {
    if (historyChanging) return;
    selectConversation(conversationId);
    const generation = ++historyGeneration;
    contextRevision.value = null;
    lastFetch = { id: conversationId, at: Date.now() };
    const { data, error, response } = await client.GET(
      "/api/conversations/{id}/messages",
      { params: { path: { id: conversationId } } },
    );
    const revisionHeader = response?.headers.get("X-Context-Revision");
    const revision =
      revisionHeader != null && CONTEXT_REVISION_PATTERN.test(revisionHeader)
        ? Number(revisionHeader)
        : null;
    // Older servers do not offer versioned fork snapshots. Preserve live
    // output during the optional terminal refresh in that case.
    if (requireRevision && revision == null) return;
    if (
      !error &&
      data &&
      generation === historyGeneration &&
      activeConversationId === conversationId
    ) {
      messages.value = storedToDisplayList(data);
      contextRevision.value = revision;
      sendRevision = revision;
      activeConversationId = conversationId;
      const tokensHeader = response?.headers.get("X-Context-Tokens");
      const tokens =
        tokensHeader != null && CONTEXT_REVISION_PATTERN.test(tokensHeader)
          ? Number(tokensHeader)
          : null;
      usageInputTokens.value = tokens ?? lastAssistantInputTokens(data);
      if (trackContextUsage) {
        seedContextUsage(data);
        if (tokens != null) recordUsageEvent({ input_tokens: tokens });
      }
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
    if (!content.trim()) return;
    const owner = beginRun(conversationId);
    if (!owner) return;
    const expectedRevision = contextRevision.value ?? sendRevision;
    let accepted = false;
    let reconnect = false;
    let failed = false;
    activeConversationId = conversationId;
    serverResumable.value = false;
    contextRevision.value = null;
    const generation = ++historyGeneration;

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
      const body: {
        content: string;
        provider_spec?: string;
        expected_revision?: number;
      } = { content };
      if (expectedRevision != null) body.expected_revision = expectedRevision;
      if (providerSpec) body.provider_spec = providerSpec;
      const res = await fetch(`/api/conversations/${conversationId}/messages`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
        signal: owner.signal,
      });
      if (!runs.owns(owner)) return;

      if (!res.ok || !res.body) {
        failureText = await sendFailureText(res);
        throw new Error(`HTTP ${res.status}`);
      }

      accepted = true;
      if (
        generation === historyGeneration &&
        activeConversationId === conversationId
      ) {
        sendRevision = null;
      }
      reconnect = !(await consumeSseStream(res, owner, (event) => {
        if (event.type === "error") failed = true;
        applyStreamEvent(event);
      }));
    } catch {
      failed = true;
      reconnect = accepted;
      showRunFailure(owner, failureText ?? "Failed to send message.");
    } finally {
      await finalizeRun(
        owner,
        !failed
          ? () => loadMessages(conversationId, !hadInterruptedCards)
          : undefined,
        reconnect ? () => attachRun(conversationId) : undefined,
      );
    }
  };

  /**
   * 恢复被中断的轮次（后端崩溃等）：修复悬空工具调用后不追加 user 消息，
   * 直接以现有历史续跑 agent loop。发送中（sending）时忽略。
   */
  const resumeTurn = async (conversationId: string) => {
    const owner = beginRun(conversationId);
    if (!owner) return;
    let accepted = false;
    let reconnect = false;
    let failed = false;

    try {
      const res = await fetch(`/api/conversations/${conversationId}/resume`, {
        method: "POST",
        signal: owner.signal,
      });
      if (!runs.owns(owner)) return;

      if (!res.ok || !res.body) {
        throw new Error(`HTTP ${res.status}`);
      }

      // 后端已把占位 tool 行持久化：先拉取让悬空卡片落定，再开始消费
      // 事件（SSE 事件在开始读取前由浏览器缓冲，不会丢失）。
      accepted = true;
      await loadMessages(conversationId);
      if (!runs.owns(owner)) return;

      const assistantId = `resumed-${Date.now()}`;
      messages.value = [
        ...messages.value,
        { id: assistantId, role: "assistant", parts: [], isStreaming: true },
      ];
      streamState = { messageId: assistantId, parts: [], isStreaming: true };
      startFlushing();

      reconnect = !(await consumeSseStream(res, owner));
    } catch {
      failed = true;
      reconnect = accepted;
      showRunFailure(owner, "Failed to resume turn.");
    } finally {
      await finalizeRun(
        owner,
        !failed ? () => loadMessages(conversationId) : undefined,
        reconnect ? () => attachRun(conversationId) : undefined,
      );
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
  async function attachRun(conversationId: string, retries = 2): Promise<void> {
    if (activeConversationId !== conversationId) return;
    const owner = beginRun(conversationId);
    if (!owner) return;
    let accepted = false;
    let reconnect = false;

    try {
      // 先探状态（404/请求失败按空闲处理，交给 finally 的静默刷新兜底）。
      const { data, error } = await client.GET("/api/conversations/{id}/run", {
        params: { path: { id: conversationId } },
      });
      if (!runs.owns(owner)) return;
      serverResumable.value = data?.resumable ?? false;
      // 切走时订阅已中止：即使 run 已空闲，也要对账可能错过的 resolved。
      // 重放的 requested/resolved 事件在快照之后叠加。
      await refreshApprovals(conversationId);
      if (!runs.owns(owner)) return;
      if (error || !data?.running) {
        // 空闲：静默刷新后解锁返回（也兜住"查状态与订阅之间刚好结束"
        // 的竞态——刷新拿到 DB 终态）。
        return;
      }

      // 快照已对账，再订阅事件流以重放并继续实时更新。
      const res = await fetch(`/api/conversations/${conversationId}/events`, {
        signal: owner.signal,
      });
      if (!runs.owns(owner)) return;
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

      accepted = true;
      reconnect = !(await consumeSseStream(res, owner, (event) => {
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
      }));
    } catch {
      reconnect = accepted;
      // 网络失败：静默（与 resumeTurn 一致的最小处理），finally 统一收敛。
    } finally {
      await finalizeRun(
        owner,
        () => loadMessages(conversationId),
        reconnect && retries > 0
          ? () => attachRun(conversationId, retries - 1)
          : undefined,
      );
    }
  }

  /**
   * 子代理面板的挂载入口：拉取子会话历史（/api/subagents/{agent_id}/
   * messages，MessageDto 同形），再探测其当前 run 的事件流（/events）。
   * 404 = 无活动 run，仅展示历史；200 则与 attachRun 同一套 run_meta
   * 锚点截断 + 事件重放 + 实时增长语义（SSE 解析/reducer 完全共用）。
   * 非终态 EOF/流读取失败时有界重连，重新按锚点重建以避免重复输出。
   * usage 只进实例本地快照（不污染主面板的全局上下文环）。
   */
  const attachSubagent = async (
    agentId: string,
    retries = 2,
  ): Promise<void> => {
    const owner = beginRun(agentId);
    if (!owner) return;
    let accepted = false;
    let reconnect = false;

    /** 拉取子会话历史并落位（不经过 fetchMessages 的会话级守卫）。 */
    const loadSubagentHistory = async () => {
      const { data, error } = await client.GET(
        "/api/subagents/{agentId}/messages",
        { params: { path: { agentId } } },
      );
      if (runs.owns(owner) && !error && data) {
        messages.value = storedToDisplayList(data);
        activeConversationId = agentId;
        usageInputTokens.value = lastAssistantInputTokens(data);
        usageWindowTokens.value = null;
      }
    };

    try {
      await loadSubagentHistory();
      if (!runs.owns(owner)) return;
      await refreshApprovals(agentId);
      if (!runs.owns(owner)) return;

      const res = await fetch(`/api/subagents/${agentId}/events`, {
        signal: owner.signal,
      });
      if (!runs.owns(owner)) return;
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

      accepted = true;
      reconnect = !(await consumeSseStream(res, owner, (event) => {
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
      }));
    } catch {
      reconnect = accepted;
      // 网络失败/主动中断：静默，finally 统一收敛（中断时跳过收尾刷新）。
    } finally {
      await finalizeRun(
        owner,
        loadSubagentHistory,
        reconnect && retries > 0
          ? () => attachSubagent(agentId, retries - 1)
          : undefined,
      );
    }
  };

  /**
   * 释放实例持有的流订阅（关闭子代理标签时调用）：中断在飞的 events
   * 订阅。只是退订——后端执行不受影响，关闭子标签不取消任务。
   */
  const dispose = () => {
    ++historyGeneration;
    disposed = true;
    approvalLoads.clear();
    runs.invalidate();
    stopFlushing();
    streamState = null;
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
    const owner = beginRun(conversationId);
    if (!owner) return;
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
      if (runs.owns(owner)) await loadMessages(conversationId);
    } catch (error) {
      console.error("compact failed", error);
    } finally {
      if (runs.owns(owner)) {
        usageCompacting.value = false;
        if (trackContextUsage) setCompacting(false);
      }
      await finalizeRun(owner);
    }
  };

  /**
   * /reload：重载指令文件与 MCP 组合——后端立即重解析并替换会话的冻结
   * 环境，下一 run 生效（运行中调用安全，不影响当前 run 的 prompt）。
   * 不触碰消息/usage，无需 beginRun；失败静默（console 记录），manifest
   * 摘要打日志作可见信号。
   */
  const reloadInstructions = async (conversationId: string) => {
    try {
      const { data, error } = await client.POST(
        "/api/conversations/{id}/instructions/reload",
        { params: { path: { id: conversationId } } },
      );
      if (error || !data) {
        console.error("instructions reload failed", error ?? "no data");
      }
      // 无 toast 基建（同 compact）：成功静默，生效信号是下一轮行为变化。
    } catch (error) {
      console.error("instructions reload failed", error);
    }
  };

  const clearMessages = () => {
    ++historyGeneration;
    serverResumable.value = false;
    messages.value = [];
    lastFetch = null;
    selectConversation(null);
    usageInputTokens.value = null;
    contextRevision.value = null;
    sendRevision = null;
    usageWindowTokens.value = null;
    usageCompacting.value = false;
    pendingApprovals.value = new Map();
    approvalLoads.clear();
    approvalVersions.clear();
    if (trackContextUsage) clearContextUsage();
  };

  // Revert can begin only with no stream owner. Invalidate older GETs and
  // prevent background wake probes or route refreshes from attaching while
  // the history transaction is in flight. A failed operation keeps the view.
  const beginHistoryChange = () => {
    if (sending.value || historyChanging) return false;
    historyChanging = true;
    ++historyGeneration;
    return true;
  };
  const endHistoryChange = async (conversationId: string, reload: boolean) => {
    historyChanging = false;
    if (!reload) return;
    clearMessages();
    lastFetch = null;
    await loadMessages(conversationId);
    const generation = historyGeneration;
    const { data } = await client.GET("/api/conversations/{id}/run", {
      params: { path: { id: conversationId } },
    });
    if (
      generation === historyGeneration &&
      activeConversationId === conversationId
    ) {
      serverResumable.value = data?.resumable ?? false;
      // Navigation may have skipped attachment while the change was pending.
      // Release the history action while the normal attachment consumes SSE.
      if (data?.running) void attachRun(conversationId);
    }
  };

  /** 存在被中断的轮次可恢复：悬空工具卡片，或最后一条消息是 user
   *（含后台任务通知行——后端把它作为 user 行插入，唤醒 run 启动失败时
   * 它就是可续跑的轮尾）。 */
  const resumable = computed(() => {
    if (serverResumable.value) return true;
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
    contextRevision: readonly(contextRevision),
    resumable,
    usage: {
      inputTokens: readonly(usageInputTokens),
      windowTokens: readonly(usageWindowTokens),
      compacting: readonly(usageCompacting),
    },
    /** 会话 id → 待决审批列表（ApprovalBanner 的数据源，按当前会话取桶）。 */
    pendingApprovals: readonly(pendingApprovals),
    fetchMessages,
    refreshMessages: loadMessages,
    sendMessage,
    resumeTurn,
    attachRun,
    attachSubagent,
    cancelConversation,
    compactConversation,
    reloadInstructions,
    decideApproval,
    refreshApprovals,
    clearMessages,
    beginHistoryChange,
    endHistoryChange,
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
