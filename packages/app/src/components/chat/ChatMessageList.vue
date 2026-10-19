<template>
  <!-- 消息列表 + 滚动容器：主面板与子代理 panel 共用的展示链路
       （ChatMessage / 思考块 / 工具卡片 / 压缩分隔条），各自传入
       实例化的 messages 与会话作用域 -->
  <div class="relative min-h-0 flex flex-1">
    <div
      ref="scrollContainer"
      class="workspace-panel-body min-w-0"
      @scroll="onScroll"
      @scrollend="onScrollEnd"
    >
      <div
        class="mx-auto max-w-[760px] pb-3 pl-12 pr-7 pt-[18px] space-y-[18px]"
      >
        <div v-for="msg in messages" :key="msg.id" :data-message-id="msg.id">
          <ChatMessage
            :message="msg"
            :conversation-id="conversationId"
            :fork-enabled="forkEnabled"
            :revert-enabled="revertEnabled"
            @fork="(cut) => $emit('fork', cut)"
            @revert="(cut) => $emit('revert', cut)"
          />
        </div>
        <div v-if="messages.length === 0" class="pt-20 text-center text-faint">
          <p class="mb-1.5 text-[15px] text-dim font-medium">
            {{ emptyTitle }}
          </p>
          <p class="text-[12.5px]">{{ emptyHint }}</p>
        </div>
      </div>
    </div>
    <ChatTurnNavigator
      :key="conversationId ?? ''"
      :messages="messages"
      @navigate="scrollToMessage"
    />
  </div>
</template>

<script setup lang="ts">
import { nextTick, ref, watch } from "vue";
import ChatMessage from "./ChatMessage.vue";
import ChatTurnNavigator from "./ChatTurnNavigator.vue";
import type { DisplayMessage } from "@/composables/useChat";

const props = withDefaults(
  defineProps<{
    messages: readonly DisplayMessage[];
    /** 「查看原文」（microcompact 副本回查）与子代理任务卡片的会话作用域。 */
    conversationId: string | null;
    emptyTitle?: string;
    emptyHint?: string;
    forkEnabled?: boolean;
    revertEnabled?: boolean;
  }>(),
  {
    emptyTitle: "Start a conversation",
    emptyHint: "Select a chat or create a new one",
  },
);

defineEmits<{
  fork: [cut: import("@/composables/chat/display").ForkCut];
  revert: [cut: import("@/composables/chat/display").ForkCut];
}>();

const scrollContainer = ref<HTMLElement | null>(null);
const followingLatest = ref(true);
let scrollingToMessage = false;

function onScroll() {
  if (scrollingToMessage) return;
  const container = scrollContainer.value;
  if (container) {
    followingLatest.value =
      container.scrollHeight - container.scrollTop - container.clientHeight <
      24;
  }
}

function onScrollEnd() {
  scrollingToMessage = false;
  onScroll();
}

function scrollToMessage(messageId: string) {
  const container = scrollContainer.value;
  const target = container?.querySelector<HTMLElement>(
    `[data-message-id="${CSS.escape(messageId)}"]`,
  );
  if (!container || !target) return;
  followingLatest.value = false;
  const top = Math.max(
    0,
    Math.min(
      container.scrollHeight - container.clientHeight,
      container.scrollTop +
        target.getBoundingClientRect().top -
        container.getBoundingClientRect().top -
        18,
    ),
  );
  scrollingToMessage = Math.abs(container.scrollTop - top) > 1;
  container.scrollTo({
    top,
    behavior: window.matchMedia("(prefers-reduced-motion: reduce)").matches
      ? "instant"
      : "smooth",
  });
}

async function scrollToBottom() {
  await nextTick();
  if (followingLatest.value && scrollContainer.value) {
    scrollContainer.value.scrollTop = scrollContainer.value.scrollHeight;
  }
}

watch(
  () => props.conversationId,
  () => {
    scrollingToMessage = false;
    followingLatest.value = true;
    scrollToBottom();
  },
);

watch(
  () => props.messages.filter((message) => message.role === "user").length,
  (count, previousCount) => {
    // 发送后历史刷新会把临时 id 换成持久化 id；只有新增提问才恢复跟随。
    if (count <= previousCount) return;
    scrollingToMessage = false;
    followingLatest.value = true;
    scrollToBottom();
  },
);

watch(
  () => props.messages.length,
  () => scrollToBottom(),
);

// 消息内容增长（文本/思考/工具参数/实时输出）时跟随滚动到底部；回看历史时暂停跟随。
watch(
  () => {
    const last = props.messages.at(-1);
    if (!last) return 0;
    let size = 0;
    for (const part of last.parts) {
      if (part.kind === "compaction") {
        size += part.summary.length;
      } else if (part.kind === "task_notification") {
        size += part.tasks.length;
      } else if (part.kind === "text" || part.kind === "reasoning") {
        size += part.text.length;
      } else if (part.kind === "image" || part.kind === "file") {
        // 附件缩略图/文件链接有固定高度，不参与文本增长估算。
        size += 0;
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
