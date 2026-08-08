<template>
  <aside
    v-if="visible"
    class="flex-shrink-0 w-[260px] flex flex-col border-l"
    style="
      border-color: var(--color-border);
      background-color: var(--color-panel);
    "
  >
    <div
      class="flex items-center justify-between px-4 py-3 border-b"
      style="border-color: var(--color-border)"
    >
      <span class="text-[1rem] font-medium" style="color: var(--color-text)"
        >Files</span
      >
      <button
        class="p-1 text-[0.875rem]"
        style="color: var(--color-muted); border-radius: var(--radius-sm)"
        @mouseenter="
          (e: MouseEvent) =>
            ((e.currentTarget as HTMLElement).style.backgroundColor =
              'var(--color-surface)')
        "
        @mouseleave="
          (e: MouseEvent) =>
            ((e.currentTarget as HTMLElement).style.backgroundColor =
              'transparent')
        "
        @click="$emit('close')"
      >
        ✕
      </button>
    </div>

    <div class="flex-1 overflow-y-auto p-2">
      <div v-for="item in files" :key="item.path" class="mb-1">
        <button
          v-if="item.type === 'folder'"
          class="w-full flex items-center gap-2 px-3 py-1.5 text-left text-[0.9rem] transition-colors"
          style="border-radius: var(--radius-md); color: var(--color-text)"
          @mouseenter="
            (e: MouseEvent) =>
              ((e.currentTarget as HTMLElement).style.backgroundColor =
                'var(--color-surface)')
          "
          @mouseleave="
            (e: MouseEvent) =>
              ((e.currentTarget as HTMLElement).style.backgroundColor =
                'transparent')
          "
          @click="$emit('toggle-folder', item.path)"
        >
          <span class="text-[0.875rem]" style="color: var(--color-muted)"
            >📁</span
          >
          <span class="truncate">{{ item.name }}</span>
        </button>

        <button
          v-else
          class="w-full flex items-center gap-2 px-3 py-1.5 text-left text-[0.9rem] transition-colors"
          style="border-radius: var(--radius-md); color: var(--color-text)"
          @mouseenter="
            (e: MouseEvent) =>
              ((e.currentTarget as HTMLElement).style.backgroundColor =
                'var(--color-surface)')
          "
          @mouseleave="
            (e: MouseEvent) =>
              ((e.currentTarget as HTMLElement).style.backgroundColor =
                'transparent')
          "
        >
          <span class="text-[0.875rem]" style="color: var(--color-muted)">
            {{
              item.status === "modified"
                ? "~"
                : item.status === "added"
                  ? "+"
                  : item.status === "deleted"
                    ? "−"
                    : "•"
            }}
          </span>
          <span
            class="truncate"
            :style="
              item.status === 'added'
                ? { color: 'var(--color-success)' }
                : item.status === 'deleted'
                  ? { color: 'var(--color-error)' }
                  : item.status === 'modified'
                    ? { color: 'var(--color-warning)' }
                    : {}
            "
          >
            {{ item.name }}
          </span>
        </button>
      </div>
    </div>
  </aside>
</template>

<script setup lang="ts">
defineProps<{
  visible: boolean;
  files: {
    type: "folder" | "file";
    name: string;
    path: string;
    status?: string;
  }[];
}>();

defineEmits<{
  close: [];
  "toggle-folder": [path: string];
}>();
</script>
