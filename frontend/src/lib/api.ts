import type { Conversation, StoredMessage, AgentConfigResponse, AgentEvent } from '@/types'

const BASE = ''

async function request<T>(path: string, options?: RequestInit): Promise<T> {
  const res = await fetch(`${BASE}${path}`, {
    headers: { 'Content-Type': 'application/json' },
    ...options,
  })
  if (!res.ok) {
    throw new Error(`API error: ${res.status} ${res.statusText}`)
  }
  if (res.status === 204) return undefined as T
  return res.json()
}

// --- Conversations ---

export async function listConversations(): Promise<Conversation[]> {
  return request('/api/conversations')
}

export async function createConversation(title?: string): Promise<Conversation> {
  return request('/api/conversations', {
    method: 'POST',
    body: JSON.stringify({ title }),
  })
}

export async function getConversation(id: string): Promise<Conversation> {
  return request(`/api/conversations/${id}`)
}

export async function deleteConversation(id: string): Promise<void> {
  return request(`/api/conversations/${id}`, { method: 'DELETE' })
}

export async function updateConversationTitle(id: string, title: string): Promise<void> {
  return request(`/api/conversations/${id}`, {
    method: 'PATCH',
    body: JSON.stringify({ title }),
  })
}

// --- Messages ---

export async function getMessages(conversationId: string): Promise<StoredMessage[]> {
  return request(`/api/conversations/${conversationId}/messages`)
}

// --- Chat (SSE) ---

export function sendMessage(
  conversationId: string,
  content: string,
  onEvent: (event: AgentEvent) => void,
  onError: (error: Error) => void,
  onDone: () => void,
): AbortController {
  const controller = new AbortController()

  fetch(`/api/conversations/${conversationId}/messages`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ content }),
    signal: controller.signal,
  })
    .then(async (res) => {
      if (!res.ok) {
        if (res.status === 409) {
          throw new Error('Agent is already running')
        }
        throw new Error(`API error: ${res.status} ${res.statusText}`)
      }
      const reader = res.body?.getReader()
      if (!reader) throw new Error('No response body')

      const decoder = new TextDecoder()
      let buffer = ''

      while (true) {
        const { done, value } = await reader.read()
        if (done) break

        buffer += decoder.decode(value, { stream: true })
        const lines = buffer.split('\n')
        buffer = lines.pop() || ''

        for (const line of lines) {
          if (line.startsWith('data: ')) {
            const data = line.slice(6).trim()
            if (!data) continue
            try {
              const event = JSON.parse(data) as AgentEvent
              onEvent(event)
            } catch {
              // Ignore parse errors for non-JSON data
            }
          }
        }
      }
      onDone()
    })
    .catch((err) => {
      if (err.name !== 'AbortError') {
        onError(err)
      }
    })

  return controller
}

// --- Config ---

export async function getConfig(): Promise<AgentConfigResponse> {
  return request('/api/config')
}

export async function updateConfig(providerSpec: string): Promise<AgentConfigResponse> {
  return request('/api/config', {
    method: 'PUT',
    body: JSON.stringify({ provider_spec: providerSpec }),
  })
}
