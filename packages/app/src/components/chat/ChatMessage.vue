<template>
  <div class="flex gap-4">
    <div
      class="flex-shrink-0 w-8 h-8 flex items-center justify-center rounded-md text-[0.875rem] font-medium"
      :class="
        message.role === 'user'
          ? 'bg-accent-7 text-accent-contrast'
          : 'bg-surface text-neutral-10'
      "
    >
      {{ message.role === "user" ? "U" : "A" }}
    </div>
    <div class="flex-1 min-w-0">
      <div class="text-[1rem] font-medium mb-1 text-neutral-10/60">
        {{ message.role === "user" ? "You" : "Oct" }}
      </div>
      <div
        class="text-[1rem] leading-relaxed whitespace-pre-wrap text-neutral-10"
      >
        {{ message.content }}
        <span v-if="message.isStreaming" class="animate-pulse">▌</span>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import type { DisplayMessage } from "@/composables/useChat";

defineProps<{
  message: DisplayMessage;
}>();
</script>
