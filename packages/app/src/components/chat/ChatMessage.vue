<template>
  <div class="flex gap-4">
    <div
      class="flex-shrink-0 w-8 h-8 flex items-center justify-center text-[0.875rem] font-medium"
      style="border-radius: var(--radius-md)"
      :style="
        message.role === 'user'
          ? {
              backgroundColor: 'var(--color-accent-9)',
              color: 'var(--color-accent-contrast)',
            }
          : {
              backgroundColor: 'var(--color-surface)',
              color: 'var(--color-text)',
            }
      "
    >
      {{ message.role === "user" ? "U" : "A" }}
    </div>
    <div class="flex-1 min-w-0">
      <div
        class="text-[1rem] font-medium mb-1"
        style="color: var(--color-muted)"
      >
        {{ message.role === "user" ? "You" : "Oct" }}
      </div>
      <div
        class="text-[1rem] leading-relaxed whitespace-pre-wrap"
        style="color: var(--color-text)"
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
