<template>
  <main
    class="workspace-panel relative min-w-0 flex-1 bg-background"
    @dragenter="onDragEnter"
    @dragover="onDragOver"
    @dragleave="onDragLeave"
    @drop="onDrop"
  >
    <!-- 拖拽遮罩：文件拖入整个面板时点亮（ZCode 的全 pane 提示同款） -->
    <div
      v-if="dragDepth > 0"
      class="pointer-events-none absolute inset-0 z-50 flex items-center justify-center bg-accent-1/70 backdrop-blur-sm"
    >
      <span
        class="rounded-full bg-accent-7 px-4 py-1.5 text-[13px] text-white shadow"
        >松开以添加附件</span
      >
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
      此会话是回退前的只读备份。
      <button
        type="button"
        class="rounded text-accent-10 control-focus hover:underline disabled:opacity-40"
        :disabled="!forkEnabled"
        @click="$emit('fork', { kind: 'head' })"
      >
        分支后继续
      </button>
    </div>
    <div v-else class="flex-none px-7 pb-3.5 pt-2.5">
      <div
        class="relative mx-auto max-w-[760px] border border-neutral-5 rounded-[10px] bg-surface transition-colors focus-within:border-accent-6"
      >
        <!-- 斜杠命令菜单：输入以 / 开头时浮于输入框上方（分组：内置命令
             + 各 MCP server 的 prompts） -->
        <!-- 附件 chips：上传中/失败的即态都浮在输入框上方 -->
        <AttachmentChips
          v-if="attachmentItems.length > 0 || attachmentNotice"
          :items="attachmentItems"
          :notice="attachmentNotice"
          @remove="removeAttachment"
          @retry="retryAttachment"
        />
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

          <!-- 思考档位：跟随当前模型的 reasoning_options；「默认」= 不带档位 -->
          <AppSelect
            v-if="reasoningOptions.length > 0"
            :model-value="selectedReasoning ?? 'default'"
            :options="[
              { value: 'default', label: '默认' },
              ...reasoningOptions,
            ]"
            placeholder="思考"
            title="思考档位"
            align="end"
            trigger-class="max-w-[96px] text-dim"
            @update:model-value="(v: string) => $emit('selectReasoning', v)"
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
            :disabled="!conversationId || forkBusy || !attachmentsReady"
            :title="attachmentsReady ? '发送（Enter）' : '等待附件上传完成'"
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
import { useAttachments } from "@/composables/useAttachments";
import { useMcpPromptComposer } from "@/composables/useMcpPromptComposer";
import { useMcpStatus } from "@/composables/useMcpStatus";
import { PERMISSION_MODE_OPTIONS } from "@/composables/useProjects";
import {
  buildSlashMenuGroups,
  builtinSlashCommands,
  filterSlashMenuGroups,
  flattenMenuItems,
  replaceSlashCommand,
  slashQueryOf,
} from "@/composables/useSlashCommands";
import ApprovalBanner from "./ApprovalBanner.vue";
import AttachmentChips from "./AttachmentChips.vue";
import ChatMessageList from "./ChatMessageList.vue";
import ContextRing from "./ContextRing.vue";
import PromptArgsDialog from "./PromptArgsDialog.vue";
import SlashMenu from "./SlashMenu.vue";
import type { components } from "@/api/schema";
import type {
  ComposerDraft,
  ReadyAttachment,
} from "@/composables/chat/attachments";
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
  /** 当前模型的思考档位选项（reasoning=false 或无数据时为空，不渲染控件）。 */
  reasoningOptions: readonly { value: string; label: string }[];
  /** 选中的思考档位（null = 「默认」，不带 reasoning 字段发送）。 */
  selectedReasoning: string | null;
  /** 当前会话的待决审批（approval_requested 事件 / pending 快照）。 */
  approvals: readonly PendingApproval[];
  /** 会话当前的工具权限模式（default/accept_edits/bypass）。 */
  permissionMode: string | null;
  forkEnabled?: boolean;
  revertEnabled?: boolean;
  readOnly?: boolean;
  forkBusy?: boolean;
  forkError?: string | null;
  draft?: ComposerDraft | null;
}>();
const emit = defineEmits<{
  /** 收到后端确认才清空正文与附件；拒绝时保留草稿。 */
  send: [
    content: string,
    attachments: ReadyAttachment[],
    onAccepted: () => void,
  ];
  stop: [];
  resume: [];
  /** /compact 命令：请求手动压缩当前会话。 */
  compact: [];
  /** /reload 命令：请求重载指令文件与 MCP 组合（冻结环境刷新）。 */
  reload: [];
  selectProvider: [providerId: string];
  selectModel: [modelId: string];
  /** 思考档位选择（default = 回到「默认」）。 */
  selectReasoning: [value: string];
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

// ---------------------------------------------------------------------------
// 附件：拖拽入队即传（ZCode 的「选完即传，发送只带 ready 引用」）；
// 发送门禁 = 全部 ready；ready 时空文本也允许发送。
// ---------------------------------------------------------------------------
const {
  items: attachmentItems,
  notice: attachmentNotice,
  allReady: attachmentsReady,
  addFiles,
  restoreDraft: restoreDraftAttachments,
  remove: removeAttachment,
  retry: retryAttachment,
  clearReady,
  readyPayload,
  reset: resetAttachments,
} = useAttachments(() => props.conversationId);

let composerGeneration = 0;
watch(
  () => props.conversationId,
  () => {
    composerGeneration += 1;
    resetAttachments();
  },
);
onBeforeUnmount(() => {
  composerGeneration += 1;
  resetAttachments();
});

/** 只有 OS 文件拖拽（types 含 "Files"）才点亮遮罩，文本拖拽不理会。 */
function dragHasFiles(event: DragEvent): boolean {
  return Array.from(event.dataTransfer?.types ?? []).includes("Files");
}
const dragDepth = ref(0);
function onDragEnter(event: DragEvent) {
  if (!dragHasFiles(event)) return;
  dragDepth.value += 1;
}
function onDragOver(event: DragEvent) {
  if (dragDepth.value === 0) return;
  event.preventDefault();
  if (event.dataTransfer) event.dataTransfer.dropEffect = "copy";
}
function onDragLeave() {
  if (dragDepth.value > 0) dragDepth.value -= 1;
}
function onDrop(event: DragEvent) {
  dragDepth.value = 0;
  const files = event.dataTransfer?.files;
  if (!files || files.length === 0) return;
  event.preventDefault();
  if (!props.conversationId || props.readOnly) return;
  addFiles(files);
}
/** 带参 prompt 的参数表单状态（非 null 即显示 Dialog）。 */
const argsPrompt = ref<{
  serverId: string;
  serverName: string;
  prompt: McpPromptSummary;
} | null>(null);
watch(
  () => props.conversationId,
  () => {
    inputMessage.value = "";
    argsPrompt.value = null;
  },
);
watch(
  () => props.draft,
  async (draft) => {
    if (!draft || draft.conversationId !== props.conversationId) return;
    composerGeneration += 1;
    inputMessage.value = draft.text;
    restoreDraftAttachments(draft.attachments ?? []);
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
    !props.conversationId ||
    props.sending ||
    props.forkBusy ||
    props.readOnly
  )
    return;
  // 上传未就绪不发（按钮已禁用，这里兜底键盘路径）。
  if (!attachmentsReady.value) return;
  const text = inputMessage.value;
  const payload = readyPayload();
  if (!text.trim() && payload.length === 0) return;
  // 斜杠命令拦截（v1 精确匹配）：/compact、/reload 不作为消息发送。
  if (text.trim() === "/compact") {
    inputMessage.value = "";
    emit("compact");
    return;
  }
  if (text.trim() === "/reload") {
    inputMessage.value = "";
    emit("reload");
    return;
  }
  const generation = composerGeneration;
  emit("send", text, payload, () => {
    // 旧会话的确认不得清掉切换后新写的草稿或新上传的附件。
    if (generation !== composerGeneration) return;
    if (inputMessage.value === text) inputMessage.value = "";
    clearReady(payload);
  });
}

// ---------------------------------------------------------------------------
// 通用斜杠命令菜单：`/` 开头触发，内置命令与 MCP prompts（按 server 分组）
// 并列；选中 MCP prompt 经后端渲染后**填入输入框**（不自动发送，让用户
// 确认/编辑后再回车）。带参 prompt 先弹参数表单。
// ---------------------------------------------------------------------------

const { servers: mcpServers } = useMcpStatus();

/** `/` 后的首词,作为菜单查询词;输入不以 `/` 开头时为 null。 */
const slashQuery = computed(() => slashQueryOf(inputMessage.value));

const {
  rendering: slashRendering,
  error: slashError,
  render: renderComposerPrompt,
} = useMcpPromptComposer(() => props.conversationId, inputMessage);

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

function handleInputKeydown(event: KeyboardEvent) {
  // IME 选词态的 keydown（上屏 Enter、候选翻页方向键）不得触发任何快捷行为；
  // keyCode 229 兜底部分浏览器/旧版 Safari。
  if (event.isComposing || event.keyCode === 229) return;
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
  const conversationId = props.conversationId;
  const text = await renderComposerPrompt(serverId, prompt.name, arguments_);
  if (text != null) {
    const input = inputMessage.value;
    await nextTick();
    if (props.conversationId !== conversationId || inputMessage.value !== input)
      return;
    inputEl.value?.focus();
    inputEl.value?.setSelectionRange(text.length, text.length);
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
