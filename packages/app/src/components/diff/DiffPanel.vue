<template>
  <aside
    v-if="visible"
    class="flex-shrink-0 w-[420px] flex flex-col border-l"
    style="
      border-color: var(--color-border);
      background-color: var(--color-panel);
    "
  >
    <div
      class="flex items-center justify-between px-4 py-3 border-b"
      style="border-color: var(--color-border)"
    >
      <div class="flex gap-1">
        <button
          v-for="tab in tabs"
          :key="tab.id"
          class="px-3 py-1.5 text-[0.9rem] font-medium transition-colors"
          style="border-radius: var(--radius-md)"
          :style="
            activeTab === tab.id
              ? {
                  backgroundColor: 'var(--color-surface)',
                  color: 'var(--color-text)',
                }
              : {
                  backgroundColor: 'transparent',
                  color: 'var(--color-muted)',
                }
          "
          @click="activeTab = tab.id"
        >
          {{ tab.label }}
        </button>
      </div>
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

    <div class="flex-1 overflow-auto p-4">
      <div v-if="activeTab === 'diff'" class="space-y-4">
        <div
          v-for="file in diffFiles"
          :key="file.path"
          class="border"
          style="
            border-color: var(--color-border);
            border-radius: var(--radius-md);
            overflow: hidden;
          "
        >
          <div
            class="px-3 py-2 text-[0.875rem] font-medium border-b"
            style="
              background-color: var(--color-surface);
              border-color: var(--color-border);
              color: var(--color-text);
              font-family: var(--font-mono);
            "
          >
            {{ file.path }}
          </div>
          <div
            class="p-3 text-[0.875rem] font-mono leading-relaxed"
            style="font-family: var(--font-mono); color: var(--color-text)"
          >
            <div
              v-for="(line, idx) in file.lines"
              :key="idx"
              class="px-2 py-0.5"
              :style="
                line.type === 'add'
                  ? {
                      backgroundColor: 'rgba(22, 163, 74, 0.08)',
                      color: 'var(--color-success)',
                    }
                  : line.type === 'remove'
                    ? {
                        backgroundColor: 'rgba(220, 38, 38, 0.08)',
                        color: 'var(--color-error)',
                      }
                    : {}
              "
            >
              <span
                style="color: var(--color-muted)"
                class="inline-block w-8 text-right mr-3 select-none"
                >{{ line.num }}</span
              >
              {{ line.content }}
            </div>
          </div>
        </div>
      </div>

      <div v-else class="text-[1rem]" style="color: var(--color-muted)">
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
