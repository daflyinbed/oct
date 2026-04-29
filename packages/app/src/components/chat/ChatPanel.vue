<template>
  <main
    class="flex-1 flex flex-col min-w-0"
    style="background-color: var(--color-bg)"
  >
    <div
      class="flex items-center justify-between px-4 py-2 border-b"
      style="border-color: var(--color-border)"
    >
      <div class="flex items-center gap-2">
        <select
          :value="selectedProviderId ?? ''"
          class="px-2 py-1 text-[0.875rem] border outline-none"
          style="
            background-color: var(--color-surface);
            border-color: var(--color-border);
            border-radius: var(--radius-md);
            color: var(--color-text);
          "
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
          class="px-2 py-1 text-[0.875rem] border outline-none"
          style="
            background-color: var(--color-surface);
            border-color: var(--color-border);
            border-radius: var(--radius-md);
            color: var(--color-text);
          "
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
        class="px-2 py-1 text-[0.875rem] font-medium"
        style="color: var(--color-accent-9)"
        @click="$emit('openSettings')"
      >
        ⚙ Providers
      </button>
    </div>

    <div ref="scrollContainer" class="flex-1 overflow-y-auto p-6">
      <div class="max-w-[75ch] mx-auto space-y-6">
        <ChatMessage v-for="msg in messages" :key="msg.id" :message="msg" />
        <div
          v-if="messages.length === 0"
          class="text-center py-20"
          style="color: var(--color-muted)"
        >
          <p class="text-[1.5rem] font-medium mb-2">Start a conversation</p>
          <p class="text-[1rem]">Select a chat or create a new one</p>
        </div>
      </div>
    </div>

    <div class="p-4 border-t" style="border-color: var(--color-border)">
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
          class="w-full resize-none px-4 py-3 pr-12 text-[1rem] outline-none border"
          style="
            background-color: var(--color-surface);
            border-color: var(--color-border);
            border-radius: var(--radius-md);
            color: var(--color-text);
          "
          @keydown.enter.prevent="handleSend"
        />
        <button
          class="absolute right-3 bottom-3 px-3 py-1.5 text-[0.9rem] font-medium transition-colors disabled:opacity-50"
          style="
            background-color: var(--color-accent-9);
            color: var(--color-accent-contrast);
            border-radius: var(--radius-md);
          "
          :disabled="!conversationId || sending"
          @mouseenter="
            (e: MouseEvent) =>
              ((e.currentTarget as HTMLElement).style.backgroundColor =
                'var(--color-accent-7)')
          "
          @mouseleave="
            (e: MouseEvent) =>
              ((e.currentTarget as HTMLElement).style.backgroundColor =
                'var(--color-accent-9)')
          "
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
