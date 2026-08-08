<template>
  <div
    class="h-9 flex items-center border-t border-neutral-5 bg-panel select-none"
  >
    <!-- Left zone: aligns with ProjectSidebar -->
    <div
      class="flex-shrink-0 flex items-center h-full border-neutral-5"
      :class="sidebarVisible ? 'w-[260px] px-4 border-r' : ''"
    >
      <button
        class="control-ghost control-focus flex items-center gap-1.5 py-1 text-[0.875rem] transition-colors"
        :class="sidebarVisible ? 'bg-neutral-2 text-neutral-10' : ''"
        title="Toggle Project Sidebar"
        @click="$emit('toggle-sidebar')"
      >
        <i-lucide-panel-left class="w-4 h-4" />
        <span class="hidden sm:inline">Explorer</span>
      </button>
    </div>

    <!-- Chat zone: aligns with ChatPanel -->
    <div class="flex items-center h-full px-4">
      <button
        class="control-ghost control-focus flex items-center gap-1.5 py-1 text-[0.875rem] transition-colors"
        :class="chatVisible ? 'bg-neutral-2 text-neutral-10' : ''"
        title="Toggle Chat Panel"
        @click="$emit('toggle-chat')"
      >
        <i-lucide-message-square class="w-4 h-4" />
        <span class="hidden sm:inline">Chat</span>
      </button>
    </div>

    <div class="flex-1" />

    <div class="flex items-center gap-1 h-full px-3">
      <button
        class="control-ghost control-focus flex items-center gap-1.5 px-2 py-1 text-[0.875rem] transition-colors"
        :class="diffVisible ? 'bg-neutral-2 text-neutral-10' : ''"
        title="Toggle Git Changes"
        @click="$emit('toggle-diff')"
      >
        <i-lucide-git-compare class="w-4 h-4" />
        <span class="hidden sm:inline">Changes</span>
      </button>

      <button
        class="control-ghost control-focus flex items-center gap-1.5 px-2 py-1 text-[0.875rem] transition-colors"
        :class="fileTreeVisible ? 'bg-neutral-2 text-neutral-10' : ''"
        title="Toggle File Tree"
        @click="$emit('toggle-file-tree')"
      >
        <i-lucide-files class="w-4 h-4" />
        <span class="hidden sm:inline">Files</span>
      </button>
    </div>
  </div>
</template>

<script setup lang="ts">
defineProps<{
  sidebarVisible: boolean;
  chatVisible: boolean;
  diffVisible: boolean;
  fileTreeVisible: boolean;
}>();

defineEmits<{
  "toggle-sidebar": [];
  "toggle-chat": [];
  "toggle-diff": [];
  "toggle-file-tree": [];
}>();
</script>
