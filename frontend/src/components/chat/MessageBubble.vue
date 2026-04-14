<script setup lang="ts">
import type { DisplayMessage } from '@/types'
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible'
import { Badge } from '@/components/ui/badge'
import { ref } from 'vue'

const props = defineProps<{
  message: DisplayMessage
}>()

function tryFormatJson(str: string): string {
  try {
    return JSON.stringify(JSON.parse(str), null, 2)
  } catch {
    return str
  }
}

const toolsExpanded = ref<Record<string, boolean>>({})

function toggleTool(id: string) {
  toolsExpanded.value[id] = !toolsExpanded.value[id]
}
</script>

<template>
  <div
    class="flex gap-3 py-4"
    :class="message.role === 'user' ? 'justify-end' : 'justify-start'"
  >
    <div
      class="max-w-[85%] rounded-lg px-4 py-3"
      :class="message.role === 'user'
        ? 'bg-primary text-primary-foreground'
        : 'bg-muted'"
    >
      <!-- Reasoning (collapsible) -->
      <Collapsible v-if="message.reasoning" class="mb-2">
        <CollapsibleTrigger class="flex items-center gap-1 text-xs text-muted-foreground cursor-pointer hover:text-foreground">
          <span>💭 Thinking...</span>
        </CollapsibleTrigger>
        <CollapsibleContent>
          <div class="mt-1 text-xs text-muted-foreground italic whitespace-pre-wrap border-l-2 border-muted-foreground/30 pl-2">
            {{ message.reasoning }}
          </div>
        </CollapsibleContent>
      </Collapsible>

      <!-- Main text content -->
      <div v-if="message.text" class="whitespace-pre-wrap text-sm">{{ message.text }}</div>

      <!-- Tool calls -->
      <div v-if="message.toolCalls?.length" class="mt-3 space-y-2">
        <div
          v-for="tc in message.toolCalls"
          :key="tc.id"
          class="rounded border bg-background p-2"
        >
          <div
            class="flex items-center gap-2 cursor-pointer text-xs font-mono"
            @click="toggleTool(tc.id)"
          >
            <Badge variant="outline" class="text-xs">🔧 {{ tc.name }}</Badge>
            <span class="text-muted-foreground">{{ toolsExpanded[tc.id] ? '▾' : '▸' }}</span>
          </div>
          <div v-if="toolsExpanded[tc.id]" class="mt-2">
            <pre class="text-xs bg-muted rounded p-2 overflow-x-auto">{{ tryFormatJson(tc.arguments) }}</pre>
            <!-- Show result if available -->
            <div
              v-for="tr in (message.toolResults || []).filter(r => r.call_id === tc.id)"
              :key="tr.call_id"
              class="mt-2"
            >
              <Badge :variant="tr.is_error ? 'destructive' : 'secondary'" class="text-xs mb-1">
                {{ tr.is_error ? '❌ Error' : '✅ Result' }}
              </Badge>
              <pre class="text-xs bg-muted rounded p-2 overflow-x-auto max-h-60">{{ tr.content }}</pre>
            </div>
          </div>
        </div>
      </div>

      <!-- Tool results without matching tool calls (e.g. tool role messages) -->
      <div
        v-if="message.role === 'tool' && message.toolResults?.length"
        class="space-y-2"
      >
        <div v-for="tr in message.toolResults" :key="tr.call_id">
          <Badge :variant="tr.is_error ? 'destructive' : 'secondary'" class="text-xs mb-1">
            {{ tr.is_error ? '❌ Error' : '✅ Result' }}
          </Badge>
          <pre class="text-xs bg-muted rounded p-2 overflow-x-auto max-h-60">{{ tr.content }}</pre>
        </div>
      </div>
    </div>
  </div>
</template>
