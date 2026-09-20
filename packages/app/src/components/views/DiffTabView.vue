<template>
  <section class="workspace-panel min-w-0 flex-1 bg-background">
    <header
      class="h-9 flex flex-none items-center gap-2 border-b border-line-soft px-4 text-[12.5px] text-neutral-10 font-semibold"
    >
      <i-lucide-git-compare class="h-3.5 w-3.5 text-dim" />
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
