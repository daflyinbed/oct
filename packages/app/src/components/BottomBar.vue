<template>
  <div
    class="h-9 flex items-center border-t select-none"
    style="
      border-color: var(--color-border);
      background-color: var(--color-panel);
    "
  >
    <!-- Left zone: aligns with ProjectSidebar -->
    <div
      class="flex-shrink-0 flex items-center h-full"
      :class="sidebarVisible ? 'w-[260px] px-4 border-r' : ''"
      style="border-color: var(--color-border)"
    >
      <button
        class="flex items-center gap-1.5 py-1 text-[0.875rem] transition-colors"
        style="border-radius: var(--radius-md); color: var(--color-muted)"
        :style="
          sidebarVisible
            ? {
                backgroundColor: 'var(--color-surface)',
                color: 'var(--color-text)',
              }
            : {}
        "
        title="Toggle Project Sidebar"
        @click="$emit('toggle-sidebar')"
        @mouseenter="
          (e: MouseEvent) =>
            ((e.currentTarget as HTMLElement).style.backgroundColor =
              'var(--color-surface)')
        "
        @mouseleave="
          (e: MouseEvent) => {
            if (!sidebarVisible)
              (e.currentTarget as HTMLElement).style.backgroundColor =
                'transparent';
            else
              (e.currentTarget as HTMLElement).style.backgroundColor =
                'var(--color-surface)';
          }
        "
      >
        <i-lucide-panel-left class="w-4 h-4" />
        <span class="hidden sm:inline">Explorer</span>
      </button>
    </div>

    <!-- Chat zone: aligns with ChatPanel -->
    <div class="flex items-center h-full px-4">
      <button
        class="flex items-center gap-1.5 py-1 text-[0.875rem] transition-colors"
        style="border-radius: var(--radius-md); color: var(--color-muted)"
        :style="
          chatVisible
            ? {
                backgroundColor: 'var(--color-surface)',
                color: 'var(--color-text)',
              }
            : {}
        "
        title="Toggle Chat Panel"
        @click="$emit('toggle-chat')"
        @mouseenter="
          (e: MouseEvent) =>
            ((e.currentTarget as HTMLElement).style.backgroundColor =
              'var(--color-surface)')
        "
        @mouseleave="
          (e: MouseEvent) => {
            if (!chatVisible)
              (e.currentTarget as HTMLElement).style.backgroundColor =
                'transparent';
            else
              (e.currentTarget as HTMLElement).style.backgroundColor =
                'var(--color-surface)';
          }
        "
      >
        <i-lucide-message-square class="w-4 h-4" />
        <span class="hidden sm:inline">Chat</span>
      </button>
    </div>

    <!-- Spacer -->
    <div class="flex-1"></div>

    <!-- Right zone: toggle buttons for right-side panels -->
    <div class="flex items-center gap-1 h-full px-3">
      <button
        class="flex items-center gap-1.5 px-2 py-1 text-[0.875rem] transition-colors"
        style="border-radius: var(--radius-md); color: var(--color-muted)"
        :style="
          diffVisible
            ? {
                backgroundColor: 'var(--color-surface)',
                color: 'var(--color-text)',
              }
            : {}
        "
        title="Toggle Git Changes"
        @click="$emit('toggle-diff')"
        @mouseenter="
          (e: MouseEvent) =>
            ((e.currentTarget as HTMLElement).style.backgroundColor =
              'var(--color-surface)')
        "
        @mouseleave="
          (e: MouseEvent) => {
            if (!diffVisible)
              (e.currentTarget as HTMLElement).style.backgroundColor =
                'transparent';
            else
              (e.currentTarget as HTMLElement).style.backgroundColor =
                'var(--color-surface)';
          }
        "
      >
        <i-lucide-git-compare class="w-4 h-4" />
        <span class="hidden sm:inline">Changes</span>
      </button>

      <button
        class="flex items-center gap-1.5 px-2 py-1 text-[0.875rem] transition-colors"
        style="border-radius: var(--radius-md); color: var(--color-muted)"
        :style="
          fileTreeVisible
            ? {
                backgroundColor: 'var(--color-surface)',
                color: 'var(--color-text)',
              }
            : {}
        "
        title="Toggle File Tree"
        @click="$emit('toggle-file-tree')"
        @mouseenter="
          (e: MouseEvent) =>
            ((e.currentTarget as HTMLElement).style.backgroundColor =
              'var(--color-surface)')
        "
        @mouseleave="
          (e: MouseEvent) => {
            if (!fileTreeVisible)
              (e.currentTarget as HTMLElement).style.backgroundColor =
                'transparent';
            else
              (e.currentTarget as HTMLElement).style.backgroundColor =
                'var(--color-surface)';
          }
        "
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
