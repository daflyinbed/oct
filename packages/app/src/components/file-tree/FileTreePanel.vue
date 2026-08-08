<template>
  <aside
    v-if="visible"
    class="flex-shrink-0 w-[260px] flex flex-col border-l border-neutral-5 bg-panel"
  >
    <div
      class="flex items-center justify-between px-4 py-3 border-b border-neutral-5"
    >
      <span class="text-[1rem] font-medium text-neutral-10">Files</span>
      <button
        class="control-ghost control-focus p-1 text-[0.875rem] rounded-sm"
        @click="$emit('close')"
      >
        ✕
      </button>
    </div>

    <div class="flex-1 overflow-y-auto p-2">
      <div v-for="item in files" :key="item.path" class="mb-1">
        <button
          v-if="item.type === 'folder'"
          class="control-ghost control-focus w-full flex items-center gap-2 px-3 py-1.5 text-left text-[0.9rem] transition-colors"
          @click="$emit('toggle-folder', item.path)"
        >
          <span class="text-[0.875rem] text-neutral-10/60">📁</span>
          <span class="truncate">{{ item.name }}</span>
        </button>

        <button
          v-else
          class="control-ghost control-focus w-full flex items-center gap-2 px-3 py-1.5 text-left text-[0.9rem] transition-colors"
        >
          <span class="text-[0.875rem] text-neutral-10/60">
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
            :class="
              item.status === 'added'
                ? 'text-success-10'
                : item.status === 'deleted'
                  ? 'text-danger-10'
                  : item.status === 'modified'
                    ? 'text-warning-10'
                    : 'text-neutral-10'
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
