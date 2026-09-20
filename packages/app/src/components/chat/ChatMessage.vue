<template>
  <!-- 用户消息：IM 式右侧气泡；助手回合：扁平无头像（对齐 redesign demo） -->
  <div v-if="message.role === 'user'" class="flex justify-end">
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
      <ToolCallCard v-else :part="part" />
    </template>
    <!-- 尚无任何内容时的流式占位光标 -->
    <div v-if="message.isStreaming && message.parts.length === 0">
      <span class="animate-pulse">▌</span>
    </div>
  </div>
</template>

<script setup lang="ts">
import ToolCallCard from "./ToolCallCard.vue";
import type { DisplayMessage, DisplayPart } from "@/composables/useChat";

defineProps<{
  message: DisplayMessage;
}>();

function partKey(part: DisplayPart, index: number): string {
  return part.kind === "tool_call" ? `c-${part.callId}` : `p-${index}`;
}
</script>
