import { computed, ref } from "vue";
import { chatApi, sendFailureText } from "./api";
import { createApprovals } from "./approvals";
import { createRunController } from "./run";
import { createSubmission } from "./submission";
import { createTimeline, ReplayBoundaryError } from "./timeline";
import { readChatStream } from "./transport";
import { createChatUsage } from "./usage";
import type { ChatTarget } from "./api";
import type { ReadyAttachment } from "./attachments";
import type { RunOwner } from "./run";
import type { Submission } from "./submission";
import type { UsageSink } from "./usage";
import type { components } from "@/api/schema";
import type { SubagentUpdatedEventData } from "@/composables/useSubagents";

interface SessionOptions {
  usageSink?: UsageSink;
  onTitleUpdated?: (conversationId: string, title: string) => void;
  onSubagentEvent?: (
    conversationId: string,
    data: SubagentUpdatedEventData,
  ) => void;
}

/** Application flows compose independent timeline, submission and approval owners. */
export function createSession(options: SessionOptions = {}) {
  const runs = createRunController();
  const usage = createChatUsage(options.usageSink);
  const timeline = createTimeline(runs, usage);
  const approvals = createApprovals();
  const serverResumable = ref(false);
  function selectConversation(id: string | null) {
    if (runs.conversationId !== id) serverResumable.value = false;
    timeline.selectConversation(id);
  }
  function beginRun(id: string) {
    if (runs.historyChanging || runs.disposed) return null;
    selectConversation(id);
    return runs.begin(id);
  }
  async function finalizeRun(
    owner: RunOwner,
    refresh?: () => Promise<unknown>,
    reconnect?: () => Promise<void>,
    requestId?: string,
  ) {
    if (!runs.owns(owner)) return;
    const sealed = timeline.sealTurn(owner);
    usage.setCompacting(false);
    if (refresh) {
      try {
        await refresh();
      } catch {
        /* persisted history converges on the next load */
      }
    }
    timeline.reconcileTurn(owner, sealed, requestId);
    if (!runs.finish(owner)) return;
    if (reconnect) await reconnect();
  }

  /** SSE framing stays in transport. Metadata, approvals and task updates
   * are routed before the message reducer, always under the stream owner. */
  async function consumeStream(
    res: Response,
    owner: RunOwner,
    target: ChatTarget,
    replay: boolean,
  ) {
    let ready = !replay;
    let failed = false;
    let recoveryProbe = false;
    let boundary: string | null = null;
    const terminal = await readChatStream(
      res,
      async (event) => {
        if (!runs.owns(owner)) return;
        if (event.type === "error") failed = true;
        if (event.type === "error" || event.type === "cancelled")
          recoveryProbe = true;
        if (event.type === "finish") serverResumable.value = false;
        if (event.type === "title_updated") {
          options.onTitleUpdated?.(owner.conversationId, event.data.title);
          return;
        }
        if (event.type === "subagent_updated") {
          options.onSubagentEvent?.(owner.conversationId, event.data);
          return;
        }
        // Approval state belongs to the stream's conversation bucket, never
        // to a message. Late requested/resolved events must not cross owners.
        if (event.type === "approval_requested") {
          approvals.upsert(owner.conversationId, event.data);
          return;
        }
        if (event.type === "approval_resolved") {
          approvals.remove(owner.conversationId, event.data.approval_id);
          return;
        }
        if (event.type === "run_meta") {
          const key = `${event.data.run_id}:${event.data.execution_epoch}`;
          if (boundary !== null)
            throw new ReplayBoundaryError("执行流包含重复的重放边界。");
          await timeline.beginReplay(owner, target, event.data);
          if (!runs.owns(owner)) return;
          boundary = key;
          ready = true;
          return;
        }
        if (!ready) {
          // Recovery may fail or be cancelled before publishing a ready
          // boundary. Preserve durable history and show that session outcome.
          if (event.type === "error")
            timeline.showRunFailure(owner, `Error: ${event.data}`);
          else if (event.type !== "cancelled" && event.type !== "finish")
            throw new ReplayBoundaryError("事件流缺少执行边界，请刷新后重试。");
          return;
        }
        for (const effect of timeline.applyEvent(owner, event))
          usage.apply(effect);
      },
      owner.signal,
    );
    if (recoveryProbe && target.kind === "conversation" && runs.owns(owner)) {
      try {
        const data = await chatApi.status(target.id);
        if (runs.owns(owner)) serverResumable.value = data?.resumable ?? false;
      } catch {
        /* status will be retried on reattachment */
      }
    }
    return { terminal, failed };
  }

  /** A rejected transport/HTTP response does not reveal whether the user
   * message and attachment claims committed. Reconcile, never replay POST. */
  async function reconcileUncertainSend(
    owner: RunOwner,
    submission: Submission,
  ) {
    let running = false;
    try {
      const data = await chatApi.status(owner.conversationId, owner.signal);
      if (!runs.owns(owner)) return false;
      running = data?.running ?? false;
      serverResumable.value = data?.resumable ?? false;
    } catch {
      /* history can confirm even when status probing fails */
    }
    if (!runs.owns(owner)) return false;
    let confirmed = false;
    try {
      await timeline.loadMessages(owner.conversationId, false, (history) => {
        confirmed = !!submission.receipt(history);
        return confirmed;
      });
    } catch {
      /* an unreachable backend keeps the uncertain draft available */
    }
    if (!runs.owns(owner) || !confirmed) return false;
    submission.confirm("receipt");
    return running;
  }

  async function sendMessage(
    conversationId: string,
    content: string,
    providerSpec?: string | null,
    reasoning?: components["schemas"]["ReasoningRequestDto"] | null,
    /** Uploaded ready chips are claimed atomically with this message. */
    attachments: ReadonlyArray<ReadyAttachment> = [],
    /** Called once after SSE acceptance or a durable receipt confirms POST. */
    onAccepted?: () => void,
  ) {
    if (!content.trim() && attachments.length === 0) return;
    const owner = beginRun(conversationId);
    if (!owner) return;
    const submission = createSubmission(
      conversationId,
      content,
      timeline.expectedSendRevision,
      providerSpec,
      reasoning,
      attachments,
      onAccepted,
    );
    // Send repairs interrupted cards on the backend. Refresh their persisted
    // placeholder results even when an older server has no revision header.
    const hadInterruptedCards = timeline.hasInterruptedCards();
    const generation = timeline.beginOptimisticTurn(
      owner,
      content,
      attachments,
    );
    serverResumable.value = false;
    let reconnect = false;
    let failed = false;
    let failureText: string | null = null;
    try {
      const res = await chatApi.send(
        conversationId,
        submission.body,
        owner.signal,
      );
      if (!runs.owns(owner)) return;
      if (!res.ok || !res.body) {
        failureText = await sendFailureText(res);
        throw new Error(`HTTP ${res.status}`);
      }
      submission.confirm("response");
      timeline.confirmResponse(owner, generation);
      const outcome = await consumeStream(
        res,
        owner,
        { kind: "conversation", id: conversationId },
        false,
      );
      failed = outcome.failed;
      reconnect = !outcome.terminal;
    } catch {
      failed = true;
      reconnect = submission.accepted;
      submission.uncertain();
      timeline.showRunFailure(owner, failureText ?? "Failed to send message.");
      if (!submission.accepted && runs.owns(owner))
        reconnect = await reconcileUncertainSend(owner, submission);
    } finally {
      await finalizeRun(
        owner,
        !failed
          ? () => timeline.loadMessages(conversationId, !hadInterruptedCards)
          : undefined,
        reconnect ? () => attachRun(conversationId) : undefined,
        submission.requestId,
      );
    }
  }

  /** Resume appends no user message; repairs precede this execution's boundary. */
  async function resumeTurn(conversationId: string) {
    const owner = beginRun(conversationId);
    if (!owner) return;
    let accepted = false;
    let reconnect = false;
    let failed = false;
    try {
      const res = await chatApi.resume(conversationId, owner.signal);
      if (!runs.owns(owner)) return;
      if (!res.ok || !res.body) throw new Error(`HTTP ${res.status}`);
      accepted = true;
      // Buffered SSE waits while repaired history is loaded. A ready metadata
      // event can require a second snapshot after asynchronous child recovery.
      await timeline.loadMessages(conversationId);
      if (!runs.owns(owner)) return;
      timeline.startStream(owner, "resumed");
      reconnect = !(
        await consumeStream(
          res,
          owner,
          { kind: "conversation", id: conversationId },
          false,
        )
      ).terminal;
    } catch {
      failed = true;
      reconnect = accepted;
      timeline.showRunFailure(owner, "Failed to resume turn.");
    } finally {
      await finalizeRun(
        owner,
        !failed ? () => timeline.loadMessages(conversationId) : undefined,
        reconnect ? () => attachRun(conversationId) : undefined,
      );
    }
  }

  /** Root and child subscriptions share replay, terminal refresh and bounded
   * reconnect. Root status probing and child events-404 remain explicit. */
  async function attachTarget(
    target: ChatTarget,
    retries: number,
  ): Promise<void> {
    if (target.kind === "conversation" && runs.conversationId !== target.id)
      return;
    const owner = beginRun(target.id);
    if (!owner) return;
    let accepted = false;
    let reconnect = false;
    let protocolFailed = false;
    try {
      if (target.kind === "subagent") await timeline.loadMessages(target);
      else {
        const data = await chatApi.status(target.id);
        if (!runs.owns(owner)) return;
        serverResumable.value = data?.resumable ?? false;
        await approvals.refreshApprovals(target.id);
        if (!runs.owns(owner) || !data?.running) return;
      }
      if (!runs.owns(owner)) return;
      if (target.kind === "subagent")
        await approvals.refreshApprovals(target.id);
      if (!runs.owns(owner)) return;
      const res = await chatApi.events(target, owner.signal);
      if (!runs.owns(owner) || res.status === 404) return;
      if (!res.ok || !res.body) throw new Error(`HTTP ${res.status}`);
      accepted = true;
      reconnect = !(await consumeStream(res, owner, target, true)).terminal;
    } catch (error) {
      reconnect = accepted && !(error instanceof ReplayBoundaryError);
      if (error instanceof ReplayBoundaryError) {
        protocolFailed = true;
        timeline.showRunFailure(owner, error.message);
      }
      // Transport failures converge via final history and bounded reconnect.
    } finally {
      await finalizeRun(
        owner,
        protocolFailed ? undefined : () => timeline.loadMessages(target),
        reconnect && retries > 0
          ? () => attachTarget(target, retries - 1)
          : undefined,
      );
    }
  }
  async function attachRun(id: string, retries = 2) {
    await attachTarget({ kind: "conversation", id }, retries);
  }
  async function attachSubagent(id: string, retries = 2) {
    await attachTarget({ kind: "subagent", id }, retries);
  }
  async function fetchMessages(id: string) {
    selectConversation(id);
    await timeline.fetchMessages(id);
  }
  async function refreshMessages(id: string) {
    selectConversation(id);
    await timeline.loadMessages(id);
  }

  /** Explicit cancellation affects execution; disposing only unsubscribes. */
  async function cancelConversation(id: string) {
    await chatApi.cancel(id);
  }
  /** Manual /compact refreshes stored markers after a synchronous response. */
  async function compactConversation(id: string) {
    const owner = beginRun(id);
    if (!owner) return;
    usage.setCompacting(true);
    try {
      const { data, error } = await chatApi.compact(id);
      if (error || !data) {
        console.error("compact failed", error ?? "no data");
        return;
      }
      if (runs.owns(owner)) await timeline.loadMessages(id);
    } catch (error) {
      console.error("compact failed", error);
    } finally {
      await finalizeRun(owner);
    }
  }
  /** /reload replaces the frozen environment for the next run; it does not
   * alter the current messages or usage and needs no exclusive run owner. */
  async function reloadInstructions(id: string) {
    try {
      const { data, error } = await chatApi.reload(id);
      if (error || !data)
        console.error("instructions reload failed", error ?? "no data");
      // No toast infrastructure: success is reflected by the next run.
    } catch (error) {
      console.error("instructions reload failed", error);
    }
  }
  function clearMessages() {
    if (runs.disposed) return;
    serverResumable.value = false;
    timeline.clear();
    approvals.clear();
  }
  function beginHistoryChange() {
    if (!runs.beginHistoryChange()) return false;
    timeline.invalidateLoads();
    return true;
  }
  async function endHistoryChange(id: string, reload: boolean) {
    runs.endHistoryChange();
    if (!reload || runs.disposed) return;
    clearMessages();
    selectConversation(id);
    const lease = runs.captureView();
    await timeline.loadMessages(id);
    if (!lease || !runs.ownsView(lease)) return;
    const data = await chatApi.status(id);
    if (runs.ownsView(lease)) {
      serverResumable.value = data?.resumable ?? false;
      // Navigation may have skipped attachment during the history transaction.
      if (data?.running) void attachRun(id);
    }
  }
  function dispose() {
    runs.dispose();
    timeline.dispose();
    approvals.dispose();
  }
  /** A trailing user/notification or interrupted tool card can resume even
   * when server eligibility has not yet been probed. */
  const resumable = computed(
    () =>
      serverResumable.value ||
      ["user", "notification"].includes(
        timeline.messages.value.at(-1)?.role ?? "",
      ) ||
      timeline.hasInterruptedCards(),
  );
  return {
    messages: timeline.messages,
    sending: runs.busy,
    contextRevision: timeline.contextRevision,
    resumable,
    usage: usage.state,
    pendingApprovals: approvals.pendingApprovals,
    fetchMessages,
    refreshMessages,
    getLastUserRequest: timeline.getLastUserRequest,
    sendMessage,
    resumeTurn,
    attachRun,
    attachSubagent,
    cancelConversation,
    compactConversation,
    reloadInstructions,
    decideApproval: approvals.decideApproval,
    refreshApprovals: approvals.refreshApprovals,
    clearMessages,
    beginHistoryChange,
    endHistoryChange,
    dispose,
  };
}
