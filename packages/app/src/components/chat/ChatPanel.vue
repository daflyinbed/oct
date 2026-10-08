<template>
  <main class="workspace-panel min-w-0 flex-1 bg-background">
    <div
      v-if="conversationId"
      class="flex flex-none items-center justify-between gap-2 border-b border-line-soft px-7 py-1.5 text-[11.5px] text-faint"
    >
      <span v-if="readOnly">回退前备份 · 只读</span>
      <span v-else-if="forkedAtRevision != null">
        <RouterLink
          v-if="forkedFrom"
          :to="`/conversation/${forkedFrom}`"
          class="text-accent-10 hover:underline"
          >查看来源会话</RouterLink
        >
        <span v-else>分支会话</span> · 与来源共用工作区文件
      </span>
      <span v-else />
      <button
        type="button"
        class="flex items-center gap-1 rounded px-2 py-1 text-dim control-focus hover:bg-neutral-2 disabled:opacity-40"
        :disabled="!forkEnabled"
        title="分支当前会话"
        @click="$emit('fork', { kind: 'head' })"
      >
        <i-lucide-git-branch class="h-3.5 w-3.5" />{{
          forkBusy ? "处理中…" : "分支当前会话"
        }}
      </button>
    </div>
    <div
      v-if="forkError"
      role="alert"
      class="flex flex-none items-center gap-2 px-7 py-2 text-[12px] text-danger-10"
    >
      <span>{{ forkError }}</span>
      <button
        type="button"
        class="underline"
        :disabled="sending || forkBusy"
        @click="$emit('refreshHistory')"
      >
        刷新历史
      </button>
    </div>
    <!-- 消息展示链路与子代理 panel 共用（ChatMessageList） -->
    <ChatMessageList
      :messages="messages"
      :conversation-id="conversationId"
      :fork-enabled="forkEnabled"
      :revert-enabled="revertEnabled"
      @fork="(cut) => $emit('fork', cut)"
      @revert="(cut) => $emit('revert', cut)"
    />

    <!-- 中断轮次的恢复提示条：悬空工具卡片 / 未获回答的 user 消息时出现 -->
    <div
      v-if="resumable && !sending && !readOnly"
      class="flex-none px-7 pt-2.5"
    >
      <div
        class="mx-auto max-w-[760px] flex items-center gap-2 border border-warning-6 rounded-[10px] bg-warning-1 px-3 py-2"
      >
        <i-lucide-rotate-ccw class="h-3.5 w-3.5 flex-none text-warning-10" />
        <span class="min-w-0 flex-1 text-[12.5px] text-dim">
          上一轮被中断，未得到回复
        </span>
        <button
          type="button"
          class="h-7 flex-none border border-line-soft rounded-[7px] px-2.5 text-[12px] text-dim control-focus hover:bg-warning-2 disabled:opacity-40"
          :disabled="!forkEnabled"
          title="从中断处分支"
          @click="$emit('fork', { kind: 'head' })"
        >
          <i-lucide-git-branch class="mr-1 inline h-3 w-3" />从中断处分支
        </button>
        <button
          type="button"
          class="h-7 flex-none rounded-[7px] bg-warning-7 px-2.5 text-[12px] text-warning-10 font-medium control-focus transition-[filter] disabled:opacity-40 hover:brightness-110"
          :disabled="forkBusy"
          title="修复中断的工具调用并继续此轮"
          @click="$emit('resume')"
        >
          恢复此轮
        </button>
      </div>
    </div>

    <!-- 审批条：pending 审批逐条卡片堆叠于输入区上方，
         decide 经 emit 交给页面层调 decideApproval -->
    <ApprovalBanner
      v-if="approvals.length > 0"
      :approvals="approvals"
      @decide="
        (approvalId, decision, pattern) =>
          $emit('decideApproval', approvalId, decision, pattern)
      "
    />

    <!-- 输入区：模型选择器内嵌在输入框内（Zed agent 面板的做法） -->
    <div v-if="readOnly" class="flex-none px-7 py-4 text-[13px] text-dim">
      此会话是只读备份。可通过「分支当前会话」创建分支后继续。
    </div>
    <div v-else class="flex-none px-7 pb-3.5 pt-2.5">
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
          :disabled="!conversationId || sending || forkBusy"
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

          <!-- 工具权限模式：三档切换，bypass 警示色 -->
          <AppSelect
            :model-value="permissionMode"
            :options="PERMISSION_MODE_OPTIONS"
            placeholder="权限"
            title="工具权限模式"
            align="end"
            :trigger-class="`max-w-[128px] ${permissionMode === 'bypass' ? 'text-warning-10' : 'text-dim'}`"
            :disabled="!conversationId"
            @update:model-value="(v: string) => $emit('setPermissionMode', v)"
          />
          <span class="mx-0.5 flex-none text-[12px] text-faint">/</span>

          <!-- 上下文占用圆环：hover 弹出容量详情 -->
          <ContextRing class="mr-1" />

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
            :disabled="!conversationId || forkBusy"
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
import { PERMISSION_MODE_OPTIONS } from "@/composables/useProjects";
import {
  buildSlashMenuGroups,
  builtinSlashCommands,
  filterSlashMenuGroups,
  flattenMenuItems,
  formatPromptMessages,
  replaceSlashCommand,
  slashQueryOf,
} from "@/composables/useSlashCommands";
import ApprovalBanner from "./ApprovalBanner.vue";
import ChatMessageList from "./ChatMessageList.vue";
import ContextRing from "./ContextRing.vue";
import PromptArgsDialog from "./PromptArgsDialog.vue";
import SlashMenu from "./SlashMenu.vue";
import type { components } from "@/api/schema";
import type {
  ApprovalDecision,
  DisplayMessage,
  PendingApproval,
} from "@/composables/useChat";
import type {
  BuiltinSlashCommand,
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
  /** 当前会话的待决审批（approval_requested 事件 / pending 快照）。 */
  approvals: readonly PendingApproval[];
  /** 会话当前的工具权限模式（default/accept_edits/bypass）。 */
  permissionMode: string | null;
  forkEnabled?: boolean;
  revertEnabled?: boolean;
  readOnly?: boolean;
  forkBusy?: boolean;
  forkError?: string | null;
  forkedFrom?: string | null;
  forkedAtRevision?: number | null;
  draft?: { conversationId: string; text: string } | null;
}>();
const emit = defineEmits<{
  send: [content: string];
  stop: [];
  resume: [];
  /** /compact 命令：请求手动压缩当前会话。 */
  compact: [];
  /** /reload 命令：请求重载指令文件与 MCP 组合（冻结环境刷新）。 */
  reload: [];
  selectProvider: [providerId: string];
  selectModel: [modelId: string];
  /** 审批卡决定：交页面层调 decideApproval。 */
  decideApproval: [
    approvalId: string,
    decision: ApprovalDecision,
    pattern?: string,
  ];
  /** 权限模式切换：交页面层 PATCH conversations。 */
  setPermissionMode: [mode: string];
  fork: [cut: components["schemas"]["ForkCut"]];
  revert: [cut: components["schemas"]["ForkCut"]];
  refreshHistory: [];
}>();

const inputMessage = ref("");
const inputEl = ref<HTMLTextAreaElement | null>(null);
watch(
  () => props.conversationId,
  () => {
    inputMessage.value = "";
  },
);
watch(
  () => props.draft,
  async (draft) => {
    if (!draft || draft.conversationId !== props.conversationId) return;
    inputMessage.value = draft.text;
    await nextTick();
    inputEl.value?.focus();
  },
  { immediate: true },
);

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
  if (
    !inputMessage.value.trim() ||
    !props.conversationId ||
    props.sending ||
    props.forkBusy ||
    props.readOnly
  )
    return;
  // 斜杠命令拦截（v1 精确匹配）：/compact、/reload 不作为消息发送。
  if (inputMessage.value.trim() === "/compact") {
    inputMessage.value = "";
    emit("compact");
    return;
  }
  if (inputMessage.value.trim() === "/reload") {
    inputMessage.value = "";
    emit("reload");
    return;
  }
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

// /compact 在此注册而非全局注册表：需要会话上下文（emit 到页面层的
// handleCompact）。选中即触发压缩（run 返回 null，不往输入框填文本）；
// 手输 /compact 发送仍走 handleSend 的精确匹配拦截，两条路等价。
const compactCommand: BuiltinSlashCommand = {
  label: "compact",
  description: "手动压缩当前会话的上下文",
  run: async () => {
    if (
      props.conversationId &&
      !props.sending &&
      !props.forkBusy &&
      !props.readOnly
    )
      emit("compact");
    return null;
  },
};
// /reload 同为就地注册：emit 到页面层调 reloadInstructions（后端立即重
// 解析并替换冻结 entry，下一 run 生效；运行中调用也安全）。
const reloadCommand: BuiltinSlashCommand = {
  label: "reload",
  description: "重载指令文件（AGENTS.md）与 MCP 工具组合，下一轮生效",
  run: async () => {
    if (props.conversationId && !props.readOnly) emit("reload");
    return null;
  },
};
const slashBuiltins = [compactCommand, reloadCommand, ...builtinSlashCommands];

const slashGroups = computed<SlashMenuGroup[]>(() =>
  filterSlashMenuGroups(
    buildSlashMenuGroups(mcpServers.value, slashBuiltins),
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
</script>
