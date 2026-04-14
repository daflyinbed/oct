<script setup lang="ts">
import type { Conversation } from '@/types'
import { Button } from '@/components/ui/button'
import { ScrollArea } from '@/components/ui/scroll-area'
import { Separator } from '@/components/ui/separator'

const props = defineProps<{
  conversations: Conversation[]
  activeId: string | null
  loading: boolean
}>()

const emit = defineEmits<{
  select: [id: string]
  create: []
  delete: [id: string]
}>()

function formatDate(dateStr: string): string {
  const d = new Date(dateStr + 'Z')
  return d.toLocaleDateString(undefined, { month: 'short', day: 'numeric' })
}
</script>

<template>
  <div class="flex flex-col h-full border-r bg-muted/30 w-64">
    <div class="p-3 flex items-center justify-between">
      <h2 class="text-sm font-semibold">Conversations</h2>
      <Button size="sm" variant="outline" @click="$emit('create')">+ New</Button>
    </div>
    <Separator />
    <ScrollArea class="flex-1">
      <div v-if="loading" class="p-4 text-sm text-muted-foreground">Loading...</div>
      <div v-else-if="conversations.length === 0" class="p-4 text-sm text-muted-foreground">
        No conversations yet.
      </div>
      <div v-else class="p-1">
        <div
          v-for="conv in conversations"
          :key="conv.id"
          class="group flex items-center gap-1 rounded-md px-3 py-2 text-sm cursor-pointer hover:bg-accent"
          :class="conv.id === activeId ? 'bg-accent' : ''"
          @click="$emit('select', conv.id)"
        >
          <div class="flex-1 truncate">
            <div class="truncate font-medium">{{ conv.title }}</div>
            <div class="text-xs text-muted-foreground">{{ formatDate(conv.updated_at) }}</div>
          </div>
          <Button
            size="sm"
            variant="ghost"
            class="opacity-0 group-hover:opacity-100 h-6 w-6 p-0 text-destructive"
            @click.stop="$emit('delete', conv.id)"
          >
            ×
          </Button>
        </div>
      </div>
    </ScrollArea>
  </div>
</template>
