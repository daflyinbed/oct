<script setup lang="ts">
import Header from '@/components/layout/Header.vue'
import Sidebar from '@/components/layout/Sidebar.vue'
import ChatView from '@/components/chat/ChatView.vue'
import { useConversations } from '@/composables/useConversations'

const {
  conversations,
  activeConversationId,
  loading,
  createConversation,
  deleteConversation,
  selectConversation,
} = useConversations()
</script>

<template>
  <div class="flex flex-col h-screen">
    <Header />
    <div class="flex flex-1 overflow-hidden">
      <Sidebar
        :conversations="conversations"
        :active-id="activeConversationId"
        :loading="loading"
        @select="selectConversation"
        @create="createConversation"
        @delete="deleteConversation"
      />
      <main class="flex-1 flex flex-col overflow-hidden">
        <ChatView
          v-if="activeConversationId"
          :conversation-id="activeConversationId"
        />
        <div v-else class="flex items-center justify-center h-full text-muted-foreground">
          <p>Select or create a conversation to get started.</p>
        </div>
      </main>
    </div>
  </div>
</template>
