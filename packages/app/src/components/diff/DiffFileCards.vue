<template>
  <div class="space-y-3">
    <div
      v-for="file in diffFiles"
      :key="file.path"
      class="overflow-hidden border border-line-soft rounded-md"
    >
      <div
        class="border-b border-line-soft bg-surface px-3 py-1.5 text-[11.5px] text-neutral-10 font-medium font-mono"
      >
        {{ file.path }}
      </div>
      <DiffLines :lines="file.lines" />
    </div>
  </div>
</template>

<script setup lang="ts">
import DiffLines from "./DiffLines.vue";

defineProps<{
  diffFiles: {
    path: string;
    lines: { num: number; type: string; content: string }[];
  }[];
}>();
</script>
