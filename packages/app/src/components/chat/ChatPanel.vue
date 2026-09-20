<template>
  <main class="workspace-panel min-w-0 flex-1 bg-background">
    <div ref="scrollContainer" class="workspace-panel-body">
      <div class="mx-auto max-w-[760px] px-7 pb-3 pt-[18px] space-y-[18px]">
        <ChatMessage v-for="msg in messages" :key="msg.id" :message="msg" />
        <div v-if="messages.length === 0" class="pt-20 text-center text-faint">
          <p class="mb-1.5 text-[15px] text-dim font-medium">
            Start a conversation
          </p>
          <p class="text-[12.5px]">Select a chat or create a new one</p>
        </div>
      </div>
    </div>

    <!-- 输入区：模型选择器内嵌在输入框内（Zed agent 面板的做法） -->
    <div class="flex-none px-7 pb-3.5 pt-2.5">
      <div
        class="mx-auto max-w-[760px] border border-neutral-5 rounded-[10px] bg-surface transition-colors focus-within:border-accent-6"
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
          class="block w-full resize-none border-none bg-transparent px-3.5 pb-1.5 pt-3 text-[13.5px] text-neutral-10 leading-[1.5] outline-none placeholder:text-faint"
          @keydown.enter.prevent="handleSend"
        />
        <div class="flex items-center gap-0.5 p-2">
          <span class="flex-1" />

          <!-- Provider / 模型选择器 -->
          <AppSelect
            :model-value="selectedProviderId"
            :options="providerOptions"
            placeholder="Provider"
            title="Provider"
            align="end"
            trigger-class="max-w-[180px] text-dim"
            @update:model-value="(v: string) => $emit('selectProvider', v)"
          />
          <span class="flex-none text-[12px] text-faint">/</span>
          <AppSelect
            :model-value="selectedModelId"
            :options="modelOptions"
            placeholder="Model"
            title="Model"
            align="end"
            trigger-class="max-w-[220px] text-neutral-10"
            @update:model-value="(v: string) => $emit('selectModel', v)"
          />

          <!-- agent 运行中切换为停止按钮 -->
          <button
            v-if="sending"
            class="ml-1 h-7 w-7 flex items-center justify-center rounded-[7px] bg-neutral-3 text-neutral-10 transition-colors hover:bg-neutral-4"
            title="停止"
            @click="$emit('stop')"
          >
            <i-lucide-square class="h-3 w-3 fill-current" />
          </button>
          <button
            v-else
            class="ml-1 h-7 w-7 flex items-center justify-center rounded-[7px] bg-accent-7 text-white transition-[filter] disabled:opacity-50 hover:brightness-110"
            :disabled="!conversationId"
            title="发送（Enter）"
            @click="handleSend"
          >
            <i-lucide-arrow-up class="h-3.5 w-3.5" />
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
import AppSelect from "@/components/ui/AppSelect.vue";
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
  stop: [];
  selectProvider: [providerId: string];
  selectModel: [modelId: string];
}>();

const inputMessage = ref("");
const scrollContainer = ref<HTMLElement | null>(null);
const inputEl = ref<HTMLTextAreaElement | null>(null);

const providersWithKey = computed(() =>
  props.providers.filter((p) => p.api_key_set),
);

const providerOptions = computed(() =>
  providersWithKey.value.map((p) => ({ value: p.id, label: p.name })),
);

const currentModels = computed((): ModelSummary[] => {
  if (!props.selectedProviderId) return [];
  const provider = props.providers.find(
    (p) => p.id === props.selectedProviderId,
  );
  if (!provider) return [];
  return provider.models.filter((m) => m.is_enabled);
});

const modelOptions = computed(() =>
  currentModels.value.map((m) => ({ value: m.model_id, label: m.name })),
);

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

// 消息内容增长（文本/工具参数/实时输出）时跟随滚动到底部
watch(
  () => {
    const last = props.messages.at(-1);
    if (!last) return 0;
    let size = 0;
    for (const part of last.parts) {
      if (part.kind === "text") {
        size += part.text.length;
      } else {
        size += part.arguments.length + part.liveOutput.droppedChars;
        for (const seg of part.liveOutput.segments) {
          size += seg.text.length;
        }
      }
    }
    return size;
  },
  () => scrollToBottom(),
);
</script>
