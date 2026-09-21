<template>
  <ChatPanel
    :messages="messages"
    :conversation-id="conversationId"
    :sending="sending"
    :resumable="resumable"
    :providers="providers"
    :selected-provider-id="selectedProviderId"
    :selected-model-id="selectedModelId"
    @send="handleSendMessage"
    @stop="handleStop"
    @resume="handleResume"
  />
</template>

<script setup lang="ts">
import { computed, watch } from "vue";
import { useRoute } from "vue-router";
import ChatPanel from "@/components/chat/ChatPanel.vue";
import { useChat } from "@/composables/useChat";
import { useProviders } from "@/composables/useProviders";

const route = useRoute();
const conversationId = computed(
  () => (route.params as { id?: string }).id ?? "",
);

const {
  messages,
  sending,
  resumable,
  fetchMessages,
  sendMessage,
  resumeTurn,
  attachRun,
  cancelConversation,
} = useChat();
const { providers, selectedProviderId, selectedModelId, getProviderSpec } =
  useProviders();

// 会话加载统一为 fetchMessages + attachRun：刷新/切会话后若 run 仍在
// 后端跑着，从 run 起点重放事件流并继续实时增长（attach 需在历史就位
// 后进行，重放锚点依赖 DB 消息 id）。
watch(
  conversationId,
  async (id) => {
    if (!id) return;
    await fetchMessages(id);
    await attachRun(id);
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
</script>
