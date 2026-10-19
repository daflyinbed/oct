<template>
  <!-- 压缩标记：全宽分隔条（非对话气泡），可展开摘要 -->
  <div v-if="message.role === 'compaction'" class="py-1">
    <CompactionDivider :part="compactionPart(message)" />
  </div>
  <!-- 后台任务通知：同为全宽分隔条（任务终态后后端插入的 user 行） -->
  <div v-else-if="message.role === 'notification'" class="py-1">
    <TaskNotificationDivider :part="notificationPart(message)" />
  </div>
  <!-- 用户消息：IM 式右侧气泡 + 气泡下操作行（复制 / 回退编辑）；助手回合：扁平无头像 -->
  <div
    v-else-if="message.role === 'user'"
    class="flex flex-col items-end gap-1"
  >
    <div
      class="max-w-[75%] border border-line-soft rounded-xl bg-panel px-3.5 py-2 text-[13px] text-neutral-10"
    >
      <div class="flex flex-col gap-1.5">
        <div
          v-if="hasText"
          class="whitespace-pre-wrap break-words leading-[1.55]"
        >
          <template v-for="(part, i) in message.parts" :key="`t-${i}`">
            <template v-if="part.kind === 'text'">{{ part.text }}</template>
            <span
              v-if="
                message.isStreaming &&
                part.kind === 'text' &&
                i === lastTextIndex
              "
              class="animate-pulse"
              >▌</span
            >
          </template>
        </div>
        <!-- 图片附件：缩略图（点击新标签页看原图） -->
        <div v-if="imageParts.length > 0" class="flex flex-wrap gap-1.5">
          <a
            v-for="part in imageParts"
            :key="part.attachmentId"
            :href="attachmentUrl(part.attachmentId)"
            target="_blank"
            rel="noopener"
            class="block max-w-[240px]"
            :title="part.filename ?? undefined"
          >
            <img
              :src="attachmentUrl(part.attachmentId)"
              :alt="part.filename ?? '附件图片'"
              loading="lazy"
              class="max-h-44 w-auto border border-line-soft rounded-lg"
            />
          </a>
        </div>
        <!-- 文本附件：文件链接在新标签页以纯文本查看。 -->
        <div v-if="fileParts.length > 0" class="flex flex-wrap gap-1.5">
          <a
            v-for="part in fileParts"
            :key="part.attachmentId"
            :href="attachmentUrl(part.attachmentId)"
            target="_blank"
            rel="noopener"
            class="max-w-[240px] min-w-0 flex items-center gap-1.5 border border-line-soft rounded-lg px-2 py-1.5 text-accent-10 hover:underline"
            :title="part.filename ?? '附件文件'"
          >
            <i-lucide-file-text class="h-4 w-4 flex-none" />
            <span class="truncate">{{ part.filename ?? "附件文件" }}</span>
          </a>
        </div>
      </div>
    </div>
    <div v-if="!message.isStreaming" class="flex items-center gap-0.5">
      <button
        type="button"
        :class="ACTION_BUTTON_CLASS"
        :title="copied ? '已复制' : '复制'"
        @click="copyMessage(message)"
      >
        <i-lucide-check v-if="copied" class="h-3.5 w-3.5" />
        <i-lucide-copy v-else class="h-3.5 w-3.5" />
      </button>
      <button
        v-if="revertEnabled && message.forkCut"
        type="button"
        :class="ACTION_BUTTON_CLASS"
        title="回退并编辑"
        @click="$emit('revert', message.forkCut!)"
      >
        <i-lucide-pencil class="h-3.5 w-3.5" />
      </button>
    </div>
  </div>
  <div v-else class="space-y-2">
    <template v-for="(part, i) in message.parts" :key="partKey(part, i)">
      <!-- 模型正文按 markdown 渲染（vue-stream-markdown）；光标只在最后一个 part 上 -->
      <MarkdownText
        v-if="part.kind === 'text'"
        :text="part.text"
        :streaming="message.isStreaming && i === message.parts.length - 1"
      />
      <ThinkingCard v-else-if="part.kind === 'reasoning'" :part="part" />
      <!-- 流式压缩事件落在当前流式消息的 parts 里（通常位于最前部） -->
      <CompactionDivider v-else-if="part.kind === 'compaction'" :part="part" />
      <!-- task 工具（子代理委派）有专属任务卡片，不走通用工具卡片 -->
      <TaskCard
        v-else-if="part.kind === 'tool_call' && part.name === 'task'"
        :part="part"
        :conversation-id="conversationId"
      />
      <ToolCallCard
        v-else-if="part.kind === 'tool_call'"
        :part="part"
        :conversation-id="conversationId"
      />
    </template>
    <!-- 尚无任何内容时的流式占位光标 -->
    <div v-if="message.isStreaming && message.parts.length === 0">
      <span class="animate-pulse">▌</span>
    </div>
    <!-- 助手回合操作行：复制正文（markdown 源）+ 从此处分支 -->
    <div v-if="!message.isStreaming" class="flex items-center gap-0.5">
      <button
        type="button"
        :class="ACTION_BUTTON_CLASS"
        :title="copied ? '已复制' : '复制'"
        @click="copyMessage(message)"
      >
        <i-lucide-check v-if="copied" class="h-3.5 w-3.5" />
        <i-lucide-copy v-else class="h-3.5 w-3.5" />
      </button>
      <button
        v-if="forkEnabled && message.forkCut"
        type="button"
        :class="ACTION_BUTTON_CLASS"
        :title="forkTitle(message)"
        @click="$emit('fork', message.forkCut!)"
      >
        <i-lucide-git-branch class="h-3.5 w-3.5" />
      </button>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed, ref } from "vue";
import CompactionDivider from "./CompactionDivider.vue";
import MarkdownText from "./MarkdownText.vue";
import TaskCard from "./TaskCard.vue";
import TaskNotificationDivider from "./TaskNotificationDivider.vue";
import ThinkingCard from "./ThinkingCard.vue";
import ToolCallCard from "./ToolCallCard.vue";
import type { FilePart, ImagePart } from "@/composables/chat/display";
import type {
  CompactionPart,
  DisplayMessage,
  DisplayPart,
  TaskNotificationPart,
} from "@/composables/useChat";

// --- 用户气泡的附件（图片区显示缩略图，文本文件显示链接） ---

const props = defineProps<{
  message: DisplayMessage;
  /** 「查看原文」（microcompact 副本回查）需要会话作用域。 */
  conversationId: string | null;
  forkEnabled?: boolean;
  revertEnabled?: boolean;
}>();

defineEmits<{
  fork: [cut: import("@/composables/chat/display").ForkCut];
  revert: [cut: import("@/composables/chat/display").ForkCut];
}>();

// 消息底部操作行的幽灵按钮样式（复制 / 编辑 / 分支共用）
const ACTION_BUTTON_CLASS =
  "flex h-6 w-6 items-center justify-center rounded text-faint control-focus transition-colors hover:bg-neutral-2 hover:text-neutral-10";

const copied = ref(false);
let copiedTimer: ReturnType<typeof setTimeout> | undefined;

const imageParts = computed(() =>
  props.message.parts.filter(
    (part): part is ImagePart => part.kind === "image",
  ),
);
const fileParts = computed(() =>
  props.message.parts.filter((part): part is FilePart => part.kind === "file"),
);
const hasText = computed(() =>
  props.message.parts.some((part) => part.kind === "text"),
);
/** 流式光标永远落在最后一个 text part 上（附件 parts 在其后）。 */
const lastTextIndex = computed(() => {
  for (let i = props.message.parts.length - 1; i >= 0; i -= 1) {
    if (props.message.parts[i]!.kind === "text") return i;
  }
  return -1;
});
function attachmentUrl(attachmentId: string): string {
  return `/api/attachments/${attachmentId}`;
}

/** 复制正文：取全部 text part（助手侧即 markdown 源文）。 */
function messageText(message: DisplayMessage): string {
  return message.parts
    .flatMap((part) => (part.kind === "text" ? [part.text] : []))
    .join("\n\n");
}

async function copyMessage(message: DisplayMessage) {
  try {
    await navigator.clipboard.writeText(messageText(message));
  } catch {
    return; // 剪贴板权限被拒等场景：静默失败，不弹反馈
  }
  copied.value = true;
  clearTimeout(copiedTimer);
  copiedTimer = setTimeout(() => (copied.value = false), 1500);
}

/** 分支按钮的提示文案与旧菜单一致：切点在中间工具组时是「这一步」。 */
function forkTitle(message: DisplayMessage): string {
  return message.forkCut?.kind === "message" &&
    message.forkCut.message_id !== message.id
    ? "从这一步后分支"
    : "从这条回复后分支";
}

function partKey(part: DisplayPart, index: number): string {
  return part.kind === "tool_call" ? `c-${part.callId}` : `p-${index}`;
}

/** compaction 角色的消息固定只含一个压缩 part。 */
function compactionPart(message: DisplayMessage): CompactionPart {
  return message.parts[0] as CompactionPart;
}

/** notification 角色的消息固定只含一个通知 part。 */
function notificationPart(message: DisplayMessage): TaskNotificationPart {
  return message.parts[0] as TaskNotificationPart;
}
</script>
