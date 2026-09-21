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
  cancelConversation,
} = useChat();
const { providers, selectedProviderId, selectedModelId, getProviderSpec } =
  useProviders();

watch(
  conversationId,
  (id) => {
    if (id) fetchMessages(id);
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
