import { ref, onMounted } from 'vue'
import type { Conversation } from '@/types'
import * as api from '@/lib/api'

export function useConversations() {
  const conversations = ref<Conversation[]>([])
  const activeConversationId = ref<string | null>(null)
  const loading = ref(false)

  async function loadConversations() {
    loading.value = true
    try {
      conversations.value = await api.listConversations()
    } finally {
      loading.value = false
    }
  }

  async function createConversation() {
    const conv = await api.createConversation()
    conversations.value.unshift(conv)
    activeConversationId.value = conv.id
    return conv
  }

  async function deleteConversation(id: string) {
    await api.deleteConversation(id)
    conversations.value = conversations.value.filter(c => c.id !== id)
    if (activeConversationId.value === id) {
      activeConversationId.value = conversations.value[0]?.id || null
    }
  }

  function selectConversation(id: string) {
    activeConversationId.value = id
  }

  onMounted(loadConversations)

  return {
    conversations,
    activeConversationId,
    loading,
    loadConversations,
    createConversation,
    deleteConversation,
    selectConversation,
  }
}
