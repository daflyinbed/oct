<template>
  <ChatPanel
    :messages="messages"
    :conversation-id="conversationId"
    :sending="sending"
    :providers="providers"
    :selected-provider-id="selectedProviderId"
    :selected-model-id="selectedModelId"
    @send="handleSendMessage"
    @select-provider="selectProvider"
    @select-model="selectModel"
    @open-settings="showSettings = true"
  />
  <ProviderSettings
    :visible="showSettings"
    :providers="providers"
    :loading="providerLoading"
    @close="showSettings = false"
    @save-key="handleSaveKey"
    @toggle-model="handleToggleModel"
  />
</template>

<script setup lang="ts">
import { computed, onMounted, ref, watch } from "vue";
import { useRoute } from "vue-router";
import ChatPanel from "@/components/chat/ChatPanel.vue";
import ProviderSettings from "@/components/settings/ProviderSettings.vue";
import { useChat } from "@/composables/useChat";
import { useProviders } from "@/composables/useProviders";

const route = useRoute();
const conversationId = computed(
  () => (route.params as { id?: string }).id ?? "",
);

const { messages, sending, fetchMessages, sendMessage } = useChat();
const {
  providers,
  loading: providerLoading,
  selectedProviderId,
  selectedModelId,
  fetchProviders,
  updateApiKey,
  toggleModelEnabled,
  selectProvider,
  selectModel,
  getProviderSpec,
} = useProviders();

const showSettings = ref(false);

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

async function handleSaveKey(providerId: string, apiKey: string) {
  await updateApiKey(providerId, apiKey);
}

async function handleToggleModel(
  providerId: string,
  modelId: string,
  enabled: boolean,
) {
  await toggleModelEnabled(providerId, modelId, enabled);
}

onMounted(() => {
  fetchProviders();
});
</script>
