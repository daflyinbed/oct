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
    @send="handleSendMessage"
    @stop="handleStop"
    @resume="handleResume"
    @compact="handleCompact"
    @select-provider="selectProvider"
    @select-model="selectModel"
    @decide-approval="handleDecideApproval"
    @set-permission-mode="handleSetPermissionMode"
  />
</template>

<script setup lang="ts">
import { computed, watch } from "vue";
import { useRoute } from "vue-router";
import ChatPanel from "@/components/chat/ChatPanel.vue";
import { useChat } from "@/composables/useChat";
import {
  normalizePermissionMode,
  useProjects,
} from "@/composables/useProjects";
import { useProviders } from "@/composables/useProviders";
import { refreshSubagents } from "@/composables/useSubagents";
import type { ApprovalDecision } from "@/composables/useChat";

const route = useRoute();
const conversationId = computed(
  () => (route.params as { id?: string }).id ?? "",
);

const {
  messages,
  sending,
  resumable,
  pendingApprovals,
  fetchMessages,
  sendMessage,
  resumeTurn,
  attachRun,
  cancelConversation,
  compactConversation,
  decideApproval,
} = useChat();
const {
  providers,
  selectedProviderId,
  selectedModelId,
  selectProvider,
  selectModel,
  getProviderSpec,
} = useProviders();

// --- 审批（ADR 0004）：当前会话的 pending 分桶喂给 ApprovalBanner -----------
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

// --- 权限模式（ADR 0004 §1.5）：会话行数据来自 useProjects 的会话列表 -------

const { conversations, setConversationPermissionMode } = useProjects();

const conversation = computed(() => {
  for (const list of conversations.value.values()) {
    const found = list.find((c) => c.id === conversationId.value);
    if (found) return found;
  }
  return null;
});

const permissionMode = computed(
  () => conversation.value?.permission_mode ?? "default",
);

function handleSetPermissionMode(mode: string) {
  if (!conversationId.value) return;
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
  if (!conversationId.value) return;
  await sendMessage(conversationId.value, content, getProviderSpec());
}

async function handleResume() {
  if (!conversationId.value) return;
  await resumeTurn(conversationId.value);
}

function handleStop() {
  if (conversationId.value) cancelConversation(conversationId.value);
}

async function handleCompact() {
  if (!conversationId.value) return;
  await compactConversation(conversationId.value);
}
</script>
