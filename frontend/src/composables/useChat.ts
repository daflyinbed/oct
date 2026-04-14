import { ref } from 'vue'
import type { DisplayMessage, ActiveToolCall, AgentEvent, StoredMessage, ContentPart } from '@/types'
import * as api from '@/lib/api'

export function useChat(conversationId: () => string | null) {
  const messages = ref<DisplayMessage[]>([])
  const isStreaming = ref(false)
  const currentToolCalls = ref<ActiveToolCall[]>([])
  let abortController: AbortController | null = null

  /** Parse a stored message into a display message. */
  function parseStoredMessage(msg: StoredMessage): DisplayMessage {
    const parts: ContentPart[] = JSON.parse(msg.parts_json)
    let text = ''
    let reasoning = ''
    const toolCalls: DisplayMessage['toolCalls'] = []
    const toolResults: DisplayMessage['toolResults'] = []

    for (const part of parts) {
      if ('Text' in part) text += part.Text
      else if ('Reasoning' in part) reasoning += part.Reasoning
      else if ('ToolCall' in part) toolCalls.push(part.ToolCall)
      else if ('ToolResult' in part) {
        const content = typeof part.ToolResult.content === 'string'
          ? part.ToolResult.content
          : JSON.stringify(part.ToolResult.content)
        toolResults.push({
          call_id: part.ToolResult.call_id,
          content,
          is_error: part.ToolResult.is_error,
        })
      }
    }

    return {
      id: msg.id,
      role: msg.role as DisplayMessage['role'],
      text,
      reasoning: reasoning || undefined,
      toolCalls: toolCalls.length > 0 ? toolCalls : undefined,
      toolResults: toolResults.length > 0 ? toolResults : undefined,
      createdAt: msg.created_at,
    }
  }

  /** Load messages from the server. */
  async function loadMessages() {
    const id = conversationId()
    if (!id) return
    const stored = await api.getMessages(id)
    messages.value = stored.map(parseStoredMessage)
  }

  /** Send a user message and stream the response. */
  async function sendMessage(content: string) {
    const id = conversationId()
    if (!id || isStreaming.value) return

    // Immediately show user message in UI
    messages.value.push({
      id: `temp-${Date.now()}`,
      role: 'user',
      text: content,
      createdAt: new Date().toISOString(),
    })

    isStreaming.value = true
    currentToolCalls.value = []

    // Create a placeholder for the assistant response
    const assistantMsg: DisplayMessage = {
      id: `stream-${Date.now()}`,
      role: 'assistant',
      text: '',
      toolCalls: [],
      toolResults: [],
      createdAt: new Date().toISOString(),
    }
    messages.value.push(assistantMsg)

    const assistantIndex = messages.value.length - 1

    abortController = api.sendMessage(
      id,
      content,
      (event: AgentEvent) => {
        const msg = messages.value[assistantIndex]
        if (!msg) return

        switch (event.type) {
          case 'text_delta':
            msg.text += event.data
            break
          case 'reasoning_delta':
            msg.reasoning = (msg.reasoning || '') + event.data
            break
          case 'tool_call_start': {
            const tc: ActiveToolCall = {
              id: event.data.id,
              name: event.data.name,
              arguments: event.data.arguments,
            }
            currentToolCalls.value.push(tc)
            if (!msg.toolCalls) msg.toolCalls = []
            msg.toolCalls.push({
              id: event.data.id,
              name: event.data.name,
              arguments: event.data.arguments,
            })
            break
          }
          case 'tool_result': {
            const tc = currentToolCalls.value.find(t => t.id === event.data.call_id)
            if (tc) {
              tc.result = { content: event.data.content, is_error: event.data.is_error }
            }
            if (!msg.toolResults) msg.toolResults = []
            msg.toolResults.push({
              call_id: event.data.call_id,
              content: event.data.content,
              is_error: event.data.is_error,
            })
            break
          }
          case 'finish':
            break
          case 'error':
            msg.text += `\n\n**Error:** ${event.data}`
            break
        }
        // Trigger reactivity
        messages.value = [...messages.value]
      },
      (error: Error) => {
        const msg = messages.value[assistantIndex]
        if (msg) {
          msg.text += `\n\n**Error:** ${error.message}`
          messages.value = [...messages.value]
        }
        isStreaming.value = false
        currentToolCalls.value = []
      },
      () => {
        isStreaming.value = false
        currentToolCalls.value = []
        // Reload messages to get properly persisted versions
        loadMessages()
      },
    )
  }

  function stopStreaming() {
    abortController?.abort()
    isStreaming.value = false
    currentToolCalls.value = []
  }

  return {
    messages,
    isStreaming,
    currentToolCalls,
    loadMessages,
    sendMessage,
    stopStreaming,
  }
}
