<template>
  <main class="workspace-panel min-w-0 flex-1 bg-background">
    <header class="workspace-panel-header justify-between border-b border-neutral-5">
      <div class="flex items-center gap-2">
        <select
          :value="selectedProviderId ?? ''"
          class="control-surface control-focus px-2 py-1 text-[0.875rem]"
          @change="
            (e: Event) => {
              const v = (e.target as HTMLSelectElement).value;
              if (v) $emit('selectProvider', v);
            }
          "
        >
          <option value="" disabled>Provider</option>
          <option v-for="p in providersWithKey" :key="p.id" :value="p.id">
            {{ p.name }}
          </option>
        </select>
        <select
          :value="selectedModelId ?? ''"
          class="control-surface control-focus px-2 py-1 text-[0.875rem]"
          @change="
            (e: Event) => {
              const v = (e.target as HTMLSelectElement).value;
              if (v) $emit('selectModel', v);
            }
          "
        >
          <option value="" disabled>Model</option>
          <option
            v-for="m in currentModels"
            :key="m.model_id"
            :value="m.model_id"
          >
            {{ m.name }}
          </option>
        </select>
      </div>
      <button
        class="control-ghost control-focus px-2 py-1 text-[0.875rem] font-medium text-accent-10"
        @click="$emit('openSettings')"
      >
        ⚙ Providers
      </button>
    </header>

    <div ref="scrollContainer" class="workspace-panel-body p-6">
      <div class="max-w-[75ch] mx-auto space-y-6">
        <ChatMessage v-for="msg in messages" :key="msg.id" :message="msg" />
        <div
          v-if="messages.length === 0"
          class="text-center py-20 text-neutral-10/60"
        >
          <p class="text-[1.5rem] font-medium mb-2">Start a conversation</p>
          <p class="text-[1rem]">Select a chat or create a new one</p>
        </div>
      </div>
    </div>

    <div class="p-4 border-t border-neutral-5">
      <div class="max-w-[75ch] mx-auto relative">
        <textarea
          v-model="inputMessage"
          rows="2"
          :placeholder="
            conversationId
              ? 'Ask Oct to do something...'
              : 'Select a conversation first...'
          "
          :disabled="!conversationId || sending"
          class="control-surface control-focus w-full resize-none px-4 py-3 pr-12 text-[1rem]"
          @keydown.enter.prevent="handleSend"
        />
        <button
          class="control-solid control-focus absolute right-3 bottom-3 px-3 py-1.5 text-[0.9rem] font-medium transition-colors disabled:opacity-50"
          :disabled="!conversationId || sending"
          @click="handleSend"
        >
          {{ sending ? "..." : "Send" }}
        </button>
      </div>
    </div>
  </main>
</template>

<script setup lang="ts">
import { computed, nextTick, ref, watch } from "vue";
import ChatMessage from "./ChatMessage.vue";
import type { components } from "@/api/schema";
import type { DisplayMessage } from "@/composables/useChat";

type ProviderResponse = components["schemas"]["ProviderResponse"];
type ModelSummary = components["schemas"]["ModelSummary"];

const props = defineProps<{
  messages: readonly DisplayMessage[];
  conversationId: string | null;
  sending: boolean;
  providers: readonly ProviderResponse[];
  selectedProviderId: string | null;
  selectedModelId: string | null;
}>();
const emit = defineEmits<{
  send: [content: string];
  selectProvider: [providerId: string];
  selectModel: [modelId: string];
  openSettings: [];
}>();

const inputMessage = ref("");
const scrollContainer = ref<HTMLElement | null>(null);

const providersWithKey = computed(() =>
  props.providers.filter((p) => p.api_key_set),
);

const currentModels = computed((): ModelSummary[] => {
  if (!props.selectedProviderId) return [];
  const provider = props.providers.find(
    (p) => p.id === props.selectedProviderId,
  );
  if (!provider) return [];
  return provider.models.filter((m) => m.is_enabled);
});

function handleSend() {
  if (!inputMessage.value.trim() || !props.conversationId || props.sending)
    return;
  emit("send", inputMessage.value);
  inputMessage.value = "";
}

async function scrollToBottom() {
  await nextTick();
  if (scrollContainer.value) {
    scrollContainer.value.scrollTop = scrollContainer.value.scrollHeight;
  }
}

watch(
  () => props.messages.length,
  () => scrollToBottom(),
);

watch(
  () => props.messages.at(-1)?.content,
  () => scrollToBottom(),
);
</script>
