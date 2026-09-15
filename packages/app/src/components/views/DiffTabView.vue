<template>
  <section class="workspace-panel min-w-0 flex-1 bg-background">
    <header
      class="h-9 flex-none flex items-center gap-2 px-4 border-b border-line-soft text-[12.5px] font-semibold text-neutral-10"
    >
      <i-lucide-git-compare class="w-3.5 h-3.5 text-dim" />
      {{ title }}
    </header>
    <div class="workspace-panel-body p-3">
      <DiffFileCards :diff-files="diffFiles" />
    </div>
  </section>
</template>

<script setup lang="ts">
import DiffFileCards from "@/components/diff/DiffFileCards.vue";

defineProps<{
  title?: string;
  diffFiles: {
    path: string;
    lines: { num: number; type: string; content: string }[];
  }[];
}>();
</script>
