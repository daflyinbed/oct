<template>
  <main class="workspace-panel min-w-0 flex-1 bg-background">
    <div ref="scrollContainer" class="workspace-panel-body">
      <div class="max-w-[760px] mx-auto px-7 pt-[18px] pb-3 space-y-[18px]">
        <ChatMessage v-for="msg in messages" :key="msg.id" :message="msg" />
        <div v-if="messages.length === 0" class="text-center pt-20 text-faint">
          <p class="text-[15px] font-medium mb-1.5 text-dim">
            Start a conversation
          </p>
          <p class="text-[12.5px]">Select a chat or create a new one</p>
        </div>
      </div>
    </div>

    <!-- 输入区：模型选择器内嵌在输入框内（Zed agent 面板的做法） -->
    <div class="flex-none px-7 pt-2.5 pb-3.5">
      <div
        class="max-w-[760px] mx-auto bg-surface border border-neutral-5 rounded-[10px] transition-colors focus-within:border-accent-6"
      >
        <textarea
          ref="inputEl"
          v-model="inputMessage"
          rows="2"
          :placeholder="
            conversationId
              ? 'Ask Oct to do something...'
              : 'Select a conversation first...'
          "
          :disabled="!conversationId || sending"
          class="w-full block resize-none bg-transparent border-none px-3.5 pt-3 pb-1.5 text-[13.5px] leading-[1.5] text-neutral-10 placeholder:text-faint outline-none"
          @keydown.enter.prevent="handleSend"
        />
        <div class="flex items-center gap-0.5 p-2">
          <span class="flex-1" />

          <!-- Provider / 模型选择器 -->
          <label
            class="inline-flex items-center h-[26px] px-2 rounded-md text-[12px] text-dim hover:bg-neutral-1 transition-colors max-w-[180px] cursor-pointer"
            title="Provider"
          >
            <select
              :value="selectedProviderId ?? ''"
              class="w-full appearance-none bg-transparent outline-none cursor-pointer truncate"
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
            <i-lucide-chevron-down class="w-3 h-3 flex-none text-faint" />
          </label>
          <span class="flex-none text-[12px] text-faint">/</span>
          <label
            class="inline-flex items-center h-[26px] px-2 rounded-md text-[12px] text-neutral-10 hover:bg-neutral-1 transition-colors max-w-[220px] cursor-pointer"
            title="Model"
          >
            <select
              :value="selectedModelId ?? ''"
              class="w-full appearance-none bg-transparent outline-none cursor-pointer truncate"
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
            <i-lucide-chevron-down class="w-3 h-3 flex-none text-faint" />
          </label>

          <button
            class="ml-1 w-7 h-7 rounded-[7px] bg-accent-7 text-white flex items-center justify-center transition-[filter] hover:brightness-110 disabled:opacity-50"
            :disabled="!conversationId || sending"
            title="发送（Enter）"
            @click="handleSend"
          >
            <i-lucide-arrow-up class="w-3.5 h-3.5" />
          </button>
        </div>
      </div>
    </div>
  </main>
</template>

<script setup lang="ts">
import {
  computed,
  nextTick,
  onBeforeUnmount,
  onMounted,
  ref,
  watch,
} from "vue";
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
}>();

const inputMessage = ref("");
const scrollContainer = ref<HTMLElement | null>(null);
const inputEl = ref<HTMLTextAreaElement | null>(null);

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

function focusComposer() {
  inputEl.value?.focus();
}

onMounted(() => {
  window.addEventListener("oct:focus-composer", focusComposer);
});

onBeforeUnmount(() => {
  window.removeEventListener("oct:focus-composer", focusComposer);
});

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
