<script setup lang="ts">
import { ref, watch, nextTick } from 'vue'
import MessageBubble from './MessageBubble.vue'
import type { DisplayMessage } from '@/types'

const props = defineProps<{
  messages: DisplayMessage[]
  isStreaming: boolean
}>()

const scrollContainer = ref<HTMLElement | null>(null)
const autoScroll = ref(true)

function scrollToBottom() {
  nextTick(() => {
    if (scrollContainer.value && autoScroll.value) {
      scrollContainer.value.scrollTop = scrollContainer.value.scrollHeight
    }
  })
}

watch(() => props.messages, scrollToBottom, { deep: true })
watch(() => props.isStreaming, scrollToBottom)
</script>

<template>
  <div ref="scrollContainer" class="flex-1 overflow-y-auto px-4">
    <div class="max-w-3xl mx-auto py-4">
      <div v-if="messages.length === 0" class="flex items-center justify-center h-full text-muted-foreground">
        <p>Start a conversation by sending a message.</p>
      </div>
      <MessageBubble
        v-for="msg in messages"
        :key="msg.id"
        :message="msg"
      />
      <div v-if="isStreaming" class="flex justify-start py-2">
        <div class="text-muted-foreground text-sm animate-pulse">● Thinking...</div>
      </div>
    </div>
  </div>
</template>
