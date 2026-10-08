<template>
  <ChatPanel
    :messages="messages"
    :conversation-id="conversationId"
    :sending="sending"
    :resumable="resumable"
    :providers="providers"
    :selected-provider-id="selectedProviderId"
    :selected-model-id="selectedModelId"
    :approvals="approvals"
    :permission-mode="permissionMode"
    :fork-enabled="
      !sending && !historyBusy && !revertSelection && contextRevision != null
    "
    :revert-enabled="
      !readOnly &&
      !sending &&
      !historyBusy &&
      !revertSelection &&
      contextRevision != null
    "
    :read-only="readOnly"
    :fork-busy="historyBusy"
    :fork-error="historyLoadError || forkError || revertError"
    :forked-from="conversation?.forked_from_conversation_id"
    :forked-at-revision="conversation?.forked_at_revision"
    :draft="forkDraft"
    @send="handleSendMessage"
    @stop="handleStop"
    @resume="handleResume"
    @compact="handleCompact"
    @reload="handleReload"
    @select-provider="selectProvider"
    @select-model="selectModel"
    @decide-approval="handleDecideApproval"
    @set-permission-mode="handleSetPermissionMode"
    @fork="handleFork"
    @revert="handleRevert"
    @refresh-history="handleRefreshHistory"
  />
  <HistoryRevertDialog
    v-if="revertSelection"
    :before-user="
      revertSelection.cut.kind === 'message' &&
      revertSelection.cut.side === 'before'
    "
    :removed-count="revertSelection.removedCount"
    :preview="revertSelection.preview"
    :submitting="revertApplying"
    :error="revertError"
    @confirm="confirmRevert"
    @cancel="revertSelection = null"
  />
</template>

<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { useRoute, useRouter } from "vue-router";
import ChatPanel from "@/components/chat/ChatPanel.vue";
import HistoryRevertDialog from "@/components/chat/HistoryRevertDialog.vue";
import { refreshBackgroundTasks } from "@/composables/useBackgroundTasks";
import { useChat } from "@/composables/useChat";
import { useConversationFork } from "@/composables/useConversationFork";
import { useConversationRevert } from "@/composables/useConversationRevert";
import {
  normalizePermissionMode,
  useProjects,
} from "@/composables/useProjects";
import { useProviders } from "@/composables/useProviders";
import { refreshSubagents } from "@/composables/useSubagents";
import type { ForkCut } from "@/composables/chat/display";
import type { ApprovalDecision } from "@/composables/useChat";

const route = useRoute();
const router = useRouter();
const { busy: forkBusy, error: forkError, fork } = useConversationFork();
const { error: revertError, revert } = useConversationRevert();
const revertApplying = ref(false);
const historyLoadError = ref<string | null>(null);
const historyBusy = computed(() => forkBusy.value || revertApplying.value);
const revertSelection = ref<{
  source: string;
  revision: number;
  cut: ForkCut;
  removedCount: number;
  preview: string;
} | null>(null);
const forkDraft = ref<{ conversationId: string; text: string } | null>(null);
const conversationId = computed(
  () => (route.params as { id?: string }).id ?? "",
);

const {
  messages,
  sending,
  contextRevision,
  resumable,
  pendingApprovals,
  fetchMessages,
  refreshMessages,
  sendMessage,
  resumeTurn,
  attachRun,
  cancelConversation,
  compactConversation,
  reloadInstructions,
  decideApproval,
  beginHistoryChange,
  endHistoryChange,
} = useChat();
const {
  providers,
  selectedProviderId,
  selectedModelId,
  selectProvider,
  selectModel,
  getProviderSpec,
} = useProviders();

// --- 审批：当前会话的 pending 分桶喂给 ApprovalBanner -----------------------
// 事件流（upsert/remove）与 attachRun 里的 pending 快照（GET 整表替换）在
// useChat 会话内维护，这里只按当前会话取桶。

const approvals = computed(
  () => pendingApprovals.value.get(conversationId.value) ?? [],
);

function handleDecideApproval(
  approvalId: string,
  decision: ApprovalDecision,
  pattern?: string,
) {
  if (!conversationId.value) return;
  void decideApproval(conversationId.value, approvalId, decision, pattern);
}

// --- 权限模式：会话行数据来自 useProjects 的会话列表 -------------------------

const { conversations, setConversationPermissionMode } = useProjects();

const conversation = computed(() => {
  for (const list of conversations.value.values()) {
    const found = list.find((c) => c.id === conversationId.value);
    if (found) return found;
  }
  return null;
});

const readOnly = computed(
  () => conversation.value?.origin_kind === "revert_backup",
);

const permissionMode = computed(
  () => conversation.value?.permission_mode ?? "default",
);

function handleSetPermissionMode(mode: string) {
  if (!conversationId.value || readOnly.value || historyBusy.value) return;
  // 归一化防御异常值；PATCH 成功后 useProjects 同步本地列表，档位标即时更新。
  void setConversationPermissionMode(
    conversationId.value,
    normalizePermissionMode(mode),
  );
}

// 会话加载统一为 fetchMessages + attachRun：刷新/切会话后若 run 仍在
// 后端跑着，从 run 起点重放事件流并继续实时增长（attach 需在历史就位
// 后进行，重放锚点依赖 DB 消息 id）。attach 完成后再拉一次子任务快照
// （与流内 subagent_updated 事件对账收敛）。
watch(
  conversationId,
  async (id) => {
    if (!id) return;
    await fetchMessages(id);
    await attachRun(id);
    void refreshSubagents(id);
  },
  { immediate: true },
);

async function handleSendMessage(content: string) {
  if (
    !conversationId.value ||
    historyBusy.value ||
    readOnly.value ||
    revertSelection.value
  )
    return;
  await sendMessage(conversationId.value, content, getProviderSpec());
}

async function handleFork(cut: ForkCut) {
  const source = conversationId.value;
  const revision = contextRevision.value;
  if (
    !source ||
    revision == null ||
    sending.value ||
    historyBusy.value ||
    revertSelection.value
  )
    return;
  const result = await fork(source, revision, cut);
  // Finishing an old request must not move a view the user has switched.
  if (!result || conversationId.value !== source) return;
  await router.push(`/conversation/${result.conversation.id}`);
  forkDraft.value = {
    conversationId: result.conversation.id,
    text: result.draft ?? "",
  };
}

async function handleResume() {
  if (
    !conversationId.value ||
    historyBusy.value ||
    readOnly.value ||
    revertSelection.value
  )
    return;
  await resumeTurn(conversationId.value);
}

function handleStop() {
  if (conversationId.value) cancelConversation(conversationId.value);
}

async function handleCompact() {
  if (
    !conversationId.value ||
    historyBusy.value ||
    readOnly.value ||
    revertSelection.value
  )
    return;
  await compactConversation(conversationId.value);
}

async function handleReload() {
  if (!conversationId.value || readOnly.value) return;
  await reloadInstructions(conversationId.value);
}

function handleRevert(cut: ForkCut) {
  const source = conversationId.value;
  const revision = contextRevision.value;
  if (
    !source ||
    revision == null ||
    sending.value ||
    historyBusy.value ||
    readOnly.value ||
    cut.kind !== "message"
  )
    return;
  const index = messages.value.findIndex(
    (message) => JSON.stringify(message.forkCut) === JSON.stringify(cut),
  );
  if (index < 0) return;
  const message = messages.value[index]!;
  historyLoadError.value = null;
  revertSelection.value = {
    source,
    revision,
    cut,
    removedCount:
      messages.value.length - index - (cut.side === "after" ? 1 : 0),
    preview: message.parts
      .map((part) =>
        part.kind === "text"
          ? part.text
          : part.kind === "tool_call"
            ? part.title || part.name
            : "",
      )
      .join("\n"),
  };
}

async function confirmRevert() {
  const selection = revertSelection.value;
  if (
    !selection ||
    selection.source !== conversationId.value ||
    historyBusy.value ||
    !beginHistoryChange()
  )
    return;
  revertApplying.value = true;
  let committed = false;
  try {
    const result = await revert(
      selection.source,
      selection.revision,
      selection.cut,
    );
    committed = !!result;
    if (!result) return;
    revertSelection.value = null;
    if (conversationId.value === selection.source)
      forkDraft.value = {
        conversationId: selection.source,
        text: result.draft ?? "",
      };
  } finally {
    const current = conversationId.value;
    const reload = committed || current !== selection.source;
    try {
      if (reload) {
        await Promise.all([
          refreshSubagents(null),
          refreshBackgroundTasks(null),
        ]);
      }
      await endHistoryChange(current, reload);
      if (
        committed &&
        current === conversationId.value &&
        contextRevision.value == null
      )
        historyLoadError.value = "回退已完成，历史加载失败，请刷新历史。";
      if (reload && current === conversationId.value) {
        await Promise.all([
          refreshSubagents(current),
          refreshBackgroundTasks(current),
        ]);
      }
    } catch {
      if (committed)
        historyLoadError.value = "回退已完成，历史加载失败，请刷新历史。";
    } finally {
      revertApplying.value = false;
    }
  }
}
async function handleRefreshHistory() {
  await refreshMessages(conversationId.value);
  if (contextRevision.value != null) historyLoadError.value = null;
}

watch(conversationId, () => {
  if (!revertApplying.value) revertSelection.value = null;
});
</script>
