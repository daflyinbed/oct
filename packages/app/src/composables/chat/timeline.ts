import { readonly, ref } from "vue";
import { chatApi } from "./api";
import {
  MICROCOMPACT_PLACEHOLDER,
  storedToDisplayList,
  unpersistedStreamTail,
} from "./display";
import {
  clonePart,
  finalizeUnconvergedCards,
  newStreamState,
  reduceStream,
} from "./stream";
import type { ChatTarget, HistorySnapshot } from "./api";
import type { ReadyAttachment } from "./attachments";
import type {
  AgentEvent,
  ApiMessage,
  DisplayMessage,
  DisplayPart,
  DisplayWork,
} from "./display";
import type { RunController, RunOwner } from "./run";
import type { StreamState } from "./stream";
import type { ChatUsage } from "./usage";

type ReplayMeta = Extract<AgentEvent, { type: "run_meta" }>["data"];
interface SealedTurn {
  triggerId: string | undefined;
  work?: DisplayWork;
  partial?: DisplayMessage;
}

export class ReplayBoundaryError extends Error {
  name = "ReplayBoundaryError";
}
const FLUSH_INTERVAL_MS = 50;

/** Owns the persisted snapshot and local overlays. No other module writes messages. */
export function createTimeline(runs: RunController, usage: ChatUsage) {
  const messages = ref<DisplayMessage[]>([]);
  const contextRevision = ref<number | null>(null);
  // Failed sends invalidate history actions while keeping their temporary
  // output visible. Retain the snapshot revision separately to guard retries.
  let sendRevision: number | null = null;
  let snapshot: HistorySnapshot | null = null;
  let historyReady = false;
  let historyGeneration = 0;
  /**
   * Dedupe guard: the route watch and tab-return refresh can fire for the
   * same conversation within one tick; collapse repeats into one request.
   */
  let lastFetch: { id: string; at: number } | null = null;
  /** Plain (non-reactive) mirror of the streaming message; flushed every tick. */
  let streamState: StreamState | null = null;
  let flushTimer: ReturnType<typeof setInterval> | null = null;
  /** Set by the reducer; avoids re-rendering unchanged state every tick. */
  let streamDirty = false;

  /** Write the buffered stream state into the reactive messages array. */
  function flushStream() {
    if (!streamState || !streamDirty || runs.disposed) return;
    streamDirty = false;
    const idx = messages.value.findIndex(
      (message) => message.id === streamState?.messageId,
    );
    if (idx === -1) return;
    messages.value[idx] = {
      ...messages.value[idx]!,
      parts: streamState.parts.map(clonePart),
      isStreaming: streamState.isStreaming,
      work: { ...streamState.work },
      startedAt: streamState.startedAt,
      finishedAt: streamState.finishedAt,
    };
  }
  function stopFlushing() {
    if (flushTimer !== null) clearInterval(flushTimer);
    flushTimer = null;
  }
  function startFlushing() {
    stopFlushing();
    streamDirty = true;
    flushStream();
    flushTimer = setInterval(flushStream, FLUSH_INTERVAL_MS);
  }
  function invalidateLoads() {
    ++historyGeneration;
  }
  function resetView() {
    invalidateLoads();
    stopFlushing();
    streamState = null;
    streamDirty = false;
    messages.value = [];
    snapshot = null;
    historyReady = false;
    contextRevision.value = null;
    sendRevision = null;
    usage.reset();
  }
  function selectConversation(id: string | null) {
    if (runs.select(id)) resetView();
  }

  /** Actually load history, bypassing fetchMessages' 200ms dedupe guard.
   * Receipt probes are conditional replacements: an unconfirmed GET must
   * retain the existing history, revision and optimistic output. */
  async function loadMessages(
    target: ChatTarget | string,
    requireRevision = false,
    shouldReplace?: (history: ApiMessage[], revision: number | null) => boolean,
  ): Promise<boolean> {
    if (runs.historyChanging || runs.disposed) return false;
    if (typeof target === "string")
      target = { kind: "conversation", id: target };
    selectConversation(target.id);
    const lease = runs.captureView();
    if (!lease) return false;
    const generation = ++historyGeneration;
    if (!shouldReplace) {
      historyReady = false;
      contextRevision.value = null;
    }
    lastFetch = { id: target.id, at: Date.now() };
    const next = await chatApi.history(target);
    // Older servers do not offer versioned fork snapshots. Preserve live
    // output during the optional terminal refresh in that case.
    if (!next || (requireRevision && next.revision == null)) return false;
    if (generation !== historyGeneration || !runs.ownsView(lease)) return false;
    if (shouldReplace && !shouldReplace(next.messages, next.revision))
      return false;
    stopFlushing();
    messages.value = storedToDisplayList(next.messages);
    snapshot = next;
    historyReady = true;
    contextRevision.value = next.revision;
    sendRevision = next.revision;
    usage.seed(next.messages, next.tokens, target.kind === "subagent");
    return true;
  }
  async function fetchMessages(id: string) {
    if (lastFetch?.id === id && Date.now() - lastFetch.at < 200) return;
    await loadMessages(id);
  }

  /** The last user request belongs only to a successfully loaded current
   * snapshot. Loading/failing history must not hydrate another selection. */
  function getLastUserRequest(conversationId: string) {
    if (
      runs.disposed ||
      runs.conversationId !== conversationId ||
      !historyReady ||
      snapshot?.conversationId !== conversationId
    )
      return null;
    for (let i = snapshot.messages.length - 1; i >= 0; i--) {
      const message = snapshot.messages[i]!;
      if (message.role === "user" && message.provider_id && message.model_id)
        return {
          providerId: message.provider_id,
          modelId: message.model_id,
          reasoning: message.details?.extra?.reasoning ?? null,
        };
    }
    return null;
  }
  function lastTrigger() {
    return messages.value.findLast(
      (message) => message.role === "user" || message.role === "notification",
    );
  }
  function startStream(
    owner: RunOwner,
    prefix: string,
    triggerId?: string | null,
  ) {
    if (!runs.owns(owner)) return;
    invalidateLoads();
    const id = `${prefix}-${owner.generation}-${Date.now()}`;
    const trigger = triggerId
      ? messages.value.find((message) => message.id === triggerId)
      : lastTrigger();
    messages.value.push({
      id,
      role: "assistant",
      parts: [],
      isStreaming: true,
    });
    streamState = newStreamState(id, trigger?.startedAt ?? Date.now());
    contextRevision.value = null;
    startFlushing();
  }
  function beginOptimisticTurn(
    owner: RunOwner,
    content: string,
    attachments: ReadonlyArray<ReadyAttachment>,
  ) {
    invalidateLoads();
    contextRevision.value = null;
    // Match persisted user display: body first, then image/file links.
    // Text attachment bytes are assembled by the backend, never read here.
    const parts: DisplayPart[] = [];
    if (content.trim()) parts.push({ kind: "text", text: content });
    for (const attachment of attachments)
      parts.push({
        kind: attachment.mimeType.toLowerCase().startsWith("image/")
          ? "image"
          : "file",
        attachmentId: attachment.attachmentId,
        filename: attachment.filename,
      });
    messages.value.push({
      id: `pending-${owner.generation}`,
      role: "user",
      parts,
      startedAt: Date.now(),
    });
    startStream(owner, "stream");
    return historyGeneration;
  }
  function confirmResponse(owner: RunOwner, generation: number) {
    if (runs.owns(owner) && generation === historyGeneration)
      sendRevision = null;
  }

  /** Keep durable work preceding THIS execution, then replay from an empty
   * overlay. Membership revisions survive COW and marker ordering changes. */
  async function beginReplay(
    owner: RunOwner,
    target: ChatTarget,
    meta: ReplayMeta,
  ) {
    if (!runs.owns(owner)) return;
    const usable = () =>
      snapshot?.conversationId === target.id &&
      (snapshot.historyEpoch == null ||
        snapshot.historyEpoch === meta.history_epoch) &&
      (snapshot.revision ??
        snapshot.messages.reduce(
          (max, message) => Math.max(max, message.appended_at_revision ?? -1),
          0,
        )) >= meta.replay_after_revision;
    if (
      !Number.isSafeInteger(meta.replay_after_revision) ||
      meta.replay_after_revision < 0 ||
      typeof meta.run_id !== "string" ||
      !meta.run_id ||
      !Number.isSafeInteger(meta.execution_epoch) ||
      meta.execution_epoch < 1 ||
      !Number.isSafeInteger(meta.history_epoch) ||
      meta.history_epoch < 0
    ) {
      throw new ReplayBoundaryError("事件流缺少有效的执行边界，请刷新后重试。");
    }
    if (!usable()) await loadMessages(target);
    if (!runs.owns(owner)) return;
    if (
      !usable() ||
      !snapshot ||
      snapshot.messages.some(
        (message) =>
          !Number.isSafeInteger(message.appended_at_revision) ||
          message.appended_at_revision! < 1,
      )
    ) {
      throw new ReplayBoundaryError("历史快照与执行边界不一致，请刷新后重试。");
    }
    const prefix = snapshot.messages.filter(
      (message) => message.appended_at_revision! <= meta.replay_after_revision,
    );
    stopFlushing();
    messages.value = storedToDisplayList(prefix);
    // Execution is confirmed; the base snapshot is no longer the live head.
    sendRevision = null;
    startStream(owner, "reattached", meta.trigger_message_id);
  }

  function applyEvent(owner: RunOwner, event: AgentEvent) {
    // A newer same-conversation snapshot may have replaced this overlay even
    // though the transport owner is still live. Its late events cannot write it.
    if (
      !runs.owns(owner) ||
      !streamState ||
      !messages.value.some((message) => message.id === streamState?.messageId)
    )
      return [];
    streamDirty = true;
    const effects = reduceStream(streamState, event, Date.now());
    for (const effect of effects) {
      if (effect.kind !== "clear_tool_result") continue;
      // Microcompact normally targets older rounds. Patch history through its
      // reactive path; the live message clone is replaced by the next flush.
      for (const message of messages.value) {
        if (message.id === streamState.messageId) continue;
        for (const part of message.parts)
          if (part.kind === "tool_call" && part.callId === effect.callId) {
            part.content = MICROCOMPACT_PLACEHOLDER;
            part.cleared = true;
            part.clearedCopyMessageId = effect.copyId;
          }
      }
    }
    return effects;
  }
  function showRunFailure(owner: RunOwner, text: string) {
    if (!runs.owns(owner)) return;
    if (streamState) {
      if (
        !messages.value.some((message) => message.id === streamState?.messageId)
      )
        return;
      streamState.work.state = "failed";
      streamState.parts.push({ kind: "text", text });
      streamDirty = true;
    } else
      messages.value.push({
        id: `error-${owner.generation}`,
        role: "assistant",
        parts: [{ kind: "text", text }],
        work: { state: "failed" },
      });
    contextRevision.value = null;
  }
  function sealTurn(owner: RunOwner): SealedTurn {
    const sealed: SealedTurn = { triggerId: lastTrigger()?.id };
    if (!runs.owns(owner)) return sealed;
    stopFlushing();
    if (streamState) {
      finalizeUnconvergedCards(streamState, Date.now());
      if (streamState.work.state === "running")
        streamState.work.state = "interrupted";
      sealed.work = { ...streamState.work };
      streamState.isStreaming = false;
      streamDirty = true;
      flushStream();
      if (sealed.work.state === "interrupted" || sealed.work.state === "failed")
        sealed.partial = messages.value.find(
          (message) => message.id === streamState?.messageId,
        );
      streamState = null;
    }
    return sealed;
  }
  function reconcileTurn(
    owner: RunOwner,
    sealed: SealedTurn,
    clientRequestId?: string,
  ) {
    // Refresh splits one stream record into multiple persisted messages. Keep
    // its real terminal state and unsaved text, scoped to its original trigger.
    // Background wake/next user rows must never inherit the previous outcome.
    const persistedTriggerId =
      clientRequestId && snapshot?.conversationId === owner.conversationId
        ? snapshot.messages.find(
            (message) =>
              message.role === "user" &&
              message.details?.extra?.client_request_id === clientRequestId,
          )?.id
        : undefined;
    if (
      !sealed.work ||
      !runs.owns(owner) ||
      lastTrigger()?.id !== (persistedTriggerId ?? sealed.triggerId)
    )
      return;
    const partial = sealed.partial;
    if (
      partial &&
      !messages.value.some((message) => message.id === partial.id)
    ) {
      const tail = unpersistedStreamTail(partial.parts, messages.value);
      if (tail.length > 0) messages.value.push({ ...partial, parts: tail });
    }
    const last = messages.value.at(-1);
    if (last?.role === "assistant") last.work = sealed.work;
  }
  function clear() {
    lastFetch = null;
    selectConversation(null);
    resetView();
  }
  function dispose() {
    invalidateLoads();
    stopFlushing();
    streamState = null;
  }
  return {
    messages: readonly(messages),
    contextRevision: readonly(contextRevision),
    selectConversation,
    loadMessages,
    fetchMessages,
    getLastUserRequest,
    invalidateLoads,
    beginOptimisticTurn,
    confirmResponse,
    startStream,
    beginReplay,
    applyEvent,
    showRunFailure,
    sealTurn,
    reconcileTurn,
    clear,
    dispose,
    get expectedSendRevision() {
      return contextRevision.value ?? sendRevision;
    },
    hasInterruptedCards: () =>
      messages.value.some((message) =>
        message.parts.some(
          (part) => part.kind === "tool_call" && part.status === "interrupted",
        ),
      ),
  };
}
