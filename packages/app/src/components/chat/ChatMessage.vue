<template>
  <!-- 压缩标记：全宽分隔条（非对话气泡），可展开摘要 -->
  <div v-if="message.role === 'compaction'" class="py-1">
    <CompactionDivider :part="compactionPart(message)" />
  </div>
  <!-- 用户消息：IM 式右侧气泡；助手回合：扁平无头像（对齐 redesign demo） -->
  <div v-else-if="message.role === 'user'" class="flex justify-end">
    <div
      class="max-w-[75%] whitespace-pre-wrap break-words border border-line-soft rounded-xl bg-panel px-3.5 py-2 text-[13px] text-neutral-10 leading-[1.55]"
    >
      <template v-for="(part, i) in message.parts" :key="`t-${i}`">
        {{ part.kind === "text" ? part.text : ""
        }}<span
          v-if="message.isStreaming && i === message.parts.length - 1"
          class="animate-pulse"
          >▌</span
        >
      </template>
    </div>
  </div>
  <div v-else class="space-y-2">
    <template v-for="(part, i) in message.parts" :key="partKey(part, i)">
      <div
        v-if="part.kind === 'text'"
        class="whitespace-pre-wrap break-words text-[13px] text-neutral-10 leading-[1.55]"
      >
        {{ part.text
        }}<span
          v-if="message.isStreaming && i === message.parts.length - 1"
          class="animate-pulse"
          >▌</span
        >
      </div>
      <ThinkingCard v-else-if="part.kind === 'reasoning'" :part="part" />
      <!-- 流式压缩事件落在当前流式消息的 parts 里（通常位于最前部） -->
      <CompactionDivider v-else-if="part.kind === 'compaction'" :part="part" />
      <!-- task 工具（子代理委派）有专属任务卡片，不走通用工具卡片 -->
      <TaskCard
        v-else-if="part.name === 'task'"
        :part="part"
        :conversation-id="conversationId"
      />
      <ToolCallCard v-else :part="part" :conversation-id="conversationId" />
    </template>
    <!-- 尚无任何内容时的流式占位光标 -->
    <div v-if="message.isStreaming && message.parts.length === 0">
      <span class="animate-pulse">▌</span>
    </div>
  </div>
</template>

<script setup lang="ts">
import CompactionDivider from "./CompactionDivider.vue";
import TaskCard from "./TaskCard.vue";
import ThinkingCard from "./ThinkingCard.vue";
import ToolCallCard from "./ToolCallCard.vue";
import type {
  CompactionPart,
  DisplayMessage,
  DisplayPart,
} from "@/composables/useChat";

defineProps<{
  message: DisplayMessage;
  /** 「查看原文」（microcompact 副本回查）需要会话作用域。 */
  conversationId: string | null;
}>();

function partKey(part: DisplayPart, index: number): string {
  return part.kind === "tool_call" ? `c-${part.callId}` : `p-${index}`;
}

/** compaction 角色的消息固定只含一个压缩 part。 */
function compactionPart(message: DisplayMessage): CompactionPart {
  return message.parts[0] as CompactionPart;
}
</script>
