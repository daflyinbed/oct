<template>
  <!-- 消息列表 + 滚动容器：主面板与子代理 panel 共用的展示链路
       （ChatMessage / 思考块 / 工具卡片 / 压缩分隔条），各自传入
       实例化的 messages 与会话作用域 -->
  <div ref="scrollContainer" class="workspace-panel-body">
    <div class="mx-auto max-w-[760px] px-7 pb-3 pt-[18px] space-y-[18px]">
      <ChatMessage
        v-for="msg in messages"
        :key="msg.id"
        :message="msg"
        :conversation-id="conversationId"
        :fork-enabled="forkEnabled"
        :revert-enabled="revertEnabled"
        @fork="(cut) => $emit('fork', cut)"
        @revert="(cut) => $emit('revert', cut)"
      />
      <div v-if="messages.length === 0" class="pt-20 text-center text-faint">
        <p class="mb-1.5 text-[15px] text-dim font-medium">
          {{ emptyTitle }}
        </p>
        <p class="text-[12.5px]">{{ emptyHint }}</p>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { nextTick, ref, watch } from "vue";
import ChatMessage from "./ChatMessage.vue";
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
