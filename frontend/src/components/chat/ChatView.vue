<script setup lang="ts">
import { watch } from 'vue'
import MessageList from './MessageList.vue'
import InputBar from './InputBar.vue'
import { useChat } from '@/composables/useChat'

const props = defineProps<{
  conversationId: string | null
}>()

const { messages, isStreaming, sendMessage, stopStreaming, loadMessages } = useChat(
  () => props.conversationId,
)

watch(
  () => props.conversationId,
  async (newId) => {
    if (newId) {
      await loadMessages()
    } else {
      messages.value = []
    }
  },
  { immediate: true },
)
</script>

<template>
  <div class="flex flex-col h-full">
    <MessageList :messages="messages" :is-streaming="isStreaming" />
    <InputBar
      :disabled="isStreaming"
      @send="sendMessage"
      @stop="stopStreaming"
    />
  </div>
</template>
