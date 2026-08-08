<template>
  <aside
    v-if="visible"
    class="flex-shrink-0 w-[420px] flex flex-col border-l border-neutral-5 bg-panel"
  >
    <div
      class="flex items-center justify-between px-4 py-3 border-b border-neutral-5"
    >
      <div class="flex gap-1">
        <button
          v-for="tab in tabs"
          :key="tab.id"
          class="control-ghost control-focus px-3 py-1.5 text-[0.9rem] font-medium transition-colors"
          :class="activeTab === tab.id ? 'bg-neutral-2 text-neutral-10' : ''"
          @click="activeTab = tab.id"
        >
          {{ tab.label }}
        </button>
      </div>
      <button
        class="control-ghost control-focus p-1 text-[0.875rem] rounded-sm"
        @click="$emit('close')"
      >
        ✕
      </button>
    </div>

    <div class="flex-1 overflow-auto p-4">
      <div v-if="activeTab === 'diff'" class="space-y-4">
        <div
          v-for="file in diffFiles"
          :key="file.path"
          class="border border-neutral-5 rounded-md overflow-hidden"
        >
          <div
            class="px-3 py-2 text-[0.875rem] font-medium border-b border-neutral-5 bg-surface text-neutral-10 font-mono"
          >
            {{ file.path }}
          </div>
          <div
            class="p-3 text-[0.875rem] font-mono leading-relaxed text-neutral-10"
          >
            <div
              v-for="(line, idx) in file.lines"
              :key="idx"
              class="px-2 py-0.5"
              :class="
                line.type === 'add'
                  ? 'bg-success-1 text-success-10'
                  : line.type === 'remove'
                    ? 'bg-danger-1 text-danger-10'
                    : ''
              "
            >
              <span
                class="inline-block w-8 text-right mr-3 text-neutral-10/60 select-none"
                >{{ line.num }}</span
              >
              {{ line.content }}
            </div>
          </div>
        </div>
      </div>

      <div v-else class="text-[1rem] text-neutral-10/60">
        Select a tab to view content
      </div>
    </div>
  </aside>
</template>

<script setup lang="ts">
import { ref } from "vue";

defineProps<{
  visible: boolean;
  diffFiles: {
    path: string;
    lines: { num: number; type: string; content: string }[];
  }[];
}>();

defineEmits<{
  close: [];
}>();

const tabs = [
  { id: "diff", label: "Diff" },
  { id: "files", label: "Files" },
];

const activeTab = ref("diff");
</script>
