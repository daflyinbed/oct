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

    <!-- 中断轮次的恢复提示条：悬空工具卡片 / 未获回答的 user 消息时出现 -->
    <div v-if="resumable && !sending" class="flex-none px-7 pt-2.5">
      <div
        class="mx-auto max-w-[760px] flex items-center gap-2 border border-warning-6 rounded-[10px] bg-warning-1 px-3 py-2"
      >
        <i-lucide-rotate-ccw class="h-3.5 w-3.5 flex-none text-warning-10" />
        <span class="min-w-0 flex-1 text-[12.5px] text-dim">
          上一轮被中断，未得到回复
        </span>
        <button
          type="button"
          class="h-7 flex-none rounded-[7px] bg-warning-7 px-2.5 text-[12px] text-warning-10 font-medium control-focus transition-[filter] hover:brightness-110"
          title="修复中断的工具调用并继续此轮"
          @click="$emit('resume')"
        >
          恢复此轮
        </button>
      </div>
    </div>

    <!-- 输入区：模型选择器内嵌在输入框内（Zed agent 面板的做法） -->
    <div class="flex-none px-7 pb-3.5 pt-2.5">
      <div
        class="relative mx-auto max-w-[760px] border border-neutral-5 rounded-[10px] bg-surface transition-colors focus-within:border-accent-6"
      >
        <!-- 斜杠命令菜单：输入以 / 开头时浮于输入框上方（分组：内置命令
             + 各 MCP server 的 prompts） -->
        <SlashMenu
          v-if="slashOpen"
          :groups="slashGroups"
          :active-key="slashActiveKey"
          @select="chooseSlashItem"
        />
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
          @keydown="handleInputKeydown"
        />
        <!-- 斜杠命令渲染状态/错误行 -->
        <div
          v-if="slashRendering || slashError"
          class="flex items-center gap-1.5 px-3.5 pb-1 text-[11.5px]"
          :class="slashError ? 'text-danger-10' : 'text-faint'"
        >
          <i-lucide-loader-circle
            v-if="slashRendering"
            class="h-3 w-3 animate-spin"
          />
          <span class="min-w-0 flex-1 truncate">{{
            slashError ?? "正在渲染 prompt…"
          }}</span>
        </div>
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

    <!-- 带参 prompt 的参数表单（reka Dialog,模态） -->
    <PromptArgsDialog
      v-if="argsPrompt"
      :server-name="argsPrompt.serverName"
      :prompt="argsPrompt.prompt"
      :submitting="slashRendering"
      @submit="submitPromptArgs"
      @cancel="closeArgsDialog"
    />
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
import { renderMcpPrompt } from "@/composables/useMcpServers";
import { useMcpStatus } from "@/composables/useMcpStatus";
import {
  buildSlashMenuGroups,
  builtinSlashCommands,
  filterSlashMenuGroups,
  flattenMenuItems,
  formatPromptMessages,
  replaceSlashCommand,
  slashQueryOf,
} from "@/composables/useSlashCommands";
import ChatMessage from "./ChatMessage.vue";
import PromptArgsDialog from "./PromptArgsDialog.vue";
import SlashMenu from "./SlashMenu.vue";
import type { components } from "@/api/schema";
import type { DisplayMessage } from "@/composables/useChat";
import type {
  SlashMenuGroup,
  SlashMenuItem,
} from "@/composables/useSlashCommands";

type ProviderResponse = components["schemas"]["ProviderResponse"];
type ModelSummary = components["schemas"]["ModelSummary"];
type McpPromptSummary = components["schemas"]["McpPromptSummary"];

const props = defineProps<{
  messages: readonly DisplayMessage[];
  conversationId: string | null;
  sending: boolean;
  /** 存在被中断的轮次（悬空工具卡片或未获回答的 user 消息）。 */
  resumable: boolean;
  providers: readonly ProviderResponse[];
  selectedProviderId: string | null;
  selectedModelId: string | null;
}>();
const emit = defineEmits<{
  send: [content: string];
  stop: [];
  resume: [];
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

// ---------------------------------------------------------------------------
// 通用斜杠命令菜单：`/` 开头触发，内置命令与 MCP prompts（按 server 分组）
// 并列；选中 MCP prompt 经后端渲染后**填入输入框**（不自动发送，让用户
// 确认/编辑后再回车）。带参 prompt 先弹参数表单。
// ---------------------------------------------------------------------------

const { servers: mcpServers } = useMcpStatus();

/** `/` 后的首词,作为菜单查询词;输入不以 `/` 开头时为 null。 */
const slashQuery = computed(() => slashQueryOf(inputMessage.value));

const slashRendering = ref(false);
const slashError = ref<string | null>(null);

const slashGroups = computed<SlashMenuGroup[]>(() =>
  filterSlashMenuGroups(
    buildSlashMenuGroups(mcpServers.value, builtinSlashCommands),
    slashQuery.value ?? "",
  ),
);
const slashItems = computed<readonly SlashMenuItem[]>(() =>
  flattenMenuItems(slashGroups.value),
);

// Escape 只关闭「本次激活」:输入重新以 / 起始(或从无到有再输入 /)时
// 重新激活一次。epoch 随每次激活自增,dismissedEpoch 记录被关掉的激活。
const slashEpoch = ref(0);
const slashDismissedEpoch = ref(-1);
watch(inputMessage, (value, old) => {
  if (value.startsWith("/") && !old.startsWith("/")) slashEpoch.value += 1;
  slashError.value = null;
});

const slashOpen = computed(
  () =>
    slashQuery.value !== null &&
    slashItems.value.length > 0 &&
    slashDismissedEpoch.value < slashEpoch.value,
);

const slashActiveIndex = ref(0);
watch(slashItems, () => {
  slashActiveIndex.value = 0;
});
const slashActiveKey = computed(
  () => slashItems.value[slashActiveIndex.value]?.key ?? null,
);

/** 带参 prompt 的参数表单状态（非 null 即显示 Dialog）。 */
const argsPrompt = ref<{
  serverId: string;
  serverName: string;
  prompt: McpPromptSummary;
} | null>(null);

function handleInputKeydown(event: KeyboardEvent) {
  if (slashOpen.value) {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      slashActiveIndex.value =
        (slashActiveIndex.value + 1) % slashItems.value.length;
      return;
    }
    if (event.key === "ArrowUp") {
      event.preventDefault();
      slashActiveIndex.value =
        (slashActiveIndex.value - 1 + slashItems.value.length) %
        slashItems.value.length;
      return;
    }
    if (event.key === "Enter" || event.key === "Tab") {
      event.preventDefault();
      const item = slashItems.value[slashActiveIndex.value];
      if (item) void chooseSlashItem(item);
      return;
    }
    if (event.key === "Escape") {
      event.preventDefault();
      slashDismissedEpoch.value = slashEpoch.value;
      return;
    }
  }
  if (event.key === "Enter") {
    event.preventDefault();
    handleSend();
  }
}

async function chooseSlashItem(item: SlashMenuItem) {
  if (item.kind === "builtin") {
    // 内置命令自行接管（可能弹自己的 UI）;返回文本则同样只填入。
    const text = await item.command.run();
    if (text != null) applySlashResult(text);
    return;
  }
  if (item.prompt.arguments.length === 0) {
    await renderPromptIntoInput(item.serverId, item.prompt, {});
    return;
  }
  argsPrompt.value = {
    serverId: item.serverId,
    serverName: item.serverName,
    prompt: item.prompt,
  };
}

async function submitPromptArgs(values: Record<string, string>) {
  if (!argsPrompt.value) return;
  const { serverId, prompt } = argsPrompt.value;
  argsPrompt.value = null;
  await renderPromptIntoInput(serverId, prompt, values);
  inputEl.value?.focus();
}

function closeArgsDialog() {
  argsPrompt.value = null;
  inputEl.value?.focus();
}

async function renderPromptIntoInput(
  serverId: string,
  prompt: McpPromptSummary,
  arguments_: Record<string, string>,
) {
  slashRendering.value = true;
  slashError.value = null;
  try {
    const { messages } = await renderMcpPrompt(
      serverId,
      prompt.name,
      arguments_,
    );
    applySlashResult(formatPromptMessages(messages));
  } catch (error) {
    slashError.value = error instanceof Error ? error.message : String(error);
  } finally {
    slashRendering.value = false;
  }
}

/** 用渲染结果替换输入框里的命令首词（`/xxx` token），保留其后的文本。 */
function applySlashResult(text: string) {
  inputMessage.value = replaceSlashCommand(inputMessage.value, text);
  nextTick(() => {
    inputEl.value?.focus();
    // 光标落在渲染文本末尾（其后可能还拼着旧文本）。
    const end = text.length;
    inputEl.value?.setSelectionRange(end, end);
  });
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

// 消息内容增长（文本/思考/工具参数/实时输出）时跟随滚动到底部
watch(
  () => {
    const last = props.messages.at(-1);
    if (!last) return 0;
    let size = 0;
    for (const part of last.parts) {
      if (part.kind === "text" || part.kind === "reasoning") {
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
