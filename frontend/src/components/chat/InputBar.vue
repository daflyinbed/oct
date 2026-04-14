<script setup lang="ts">
import { ref } from 'vue'
import { Textarea } from '@/components/ui/textarea'
import { Button } from '@/components/ui/button'

const props = defineProps<{
  disabled: boolean
}>()

const emit = defineEmits<{
  send: [content: string]
  stop: []
}>()

const input = ref('')

function handleSend() {
  const text = input.value.trim()
  if (!text || props.disabled) return
  emit('send', text)
  input.value = ''
}

function handleKeydown(e: KeyboardEvent) {
  if (e.key === 'Enter' && !e.shiftKey) {
    e.preventDefault()
    handleSend()
  }
}
</script>

<template>
  <div class="border-t bg-background px-4 py-3">
    <div class="max-w-3xl mx-auto flex gap-2 items-end">
      <Textarea
        v-model="input"
        placeholder="Type a message... (Enter to send, Shift+Enter for new line)"
        class="min-h-[44px] max-h-[200px] resize-none flex-1"
        :disabled="disabled"
        @keydown="handleKeydown"
      />
      <Button
        v-if="!disabled"
        @click="handleSend"
        :disabled="!input.trim()"
        size="sm"
      >
        Send
      </Button>
      <Button
        v-else
        @click="$emit('stop')"
        variant="destructive"
        size="sm"
      >
        Stop
      </Button>
    </div>
  </div>
</template>
