<template>
  <!-- 用户消息：IM 式右侧气泡；助手回合：扁平无头像（对齐 redesign demo） -->
  <div v-if="message.role === 'user'" class="flex justify-end">
    <div
      class="max-w-[75%] bg-panel border border-line-soft rounded-xl px-3.5 py-2 text-[13px] leading-[1.55] text-neutral-10 whitespace-pre-wrap break-words"
    >
      {{ message.content }}
      <span v-if="message.isStreaming" class="animate-pulse">▌</span>
    </div>
  </div>
  <div
    v-else
    class="text-[13px] leading-[1.55] text-neutral-10 whitespace-pre-wrap break-words"
  >
    {{ message.content }}
    <span v-if="message.isStreaming" class="animate-pulse">▌</span>
  </div>
</template>

<script setup lang="ts">
import type { DisplayMessage } from "@/composables/useChat";

defineProps<{
  message: DisplayMessage;
}>();
</script>
