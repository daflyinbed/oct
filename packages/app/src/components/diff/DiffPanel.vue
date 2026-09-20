<template>
  <aside v-if="visible" class="workspace-panel h-full w-full bg-panel">
    <header
      class="workspace-panel-header select-none justify-between border-b border-line-soft"
    >
      <div
        class="flex items-center gap-[7px] text-[12.5px] text-neutral-10 font-semibold"
      >
        <i-lucide-git-compare class="h-3.5 w-3.5 text-dim" />
        Changes
      </div>
      <button
        class="h-[22px] w-[22px] control-ghost rounded-md text-[12px] control-focus"
        title="关闭"
        @click="$emit('close')"
      >
        ✕
      </button>
    </header>

    <div class="workspace-panel-body p-3">
      <DiffFileCards :diff-files="diffFiles" />
    </div>
  </aside>
</template>

<script setup lang="ts">
import DiffFileCards from "./DiffFileCards.vue";

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
</script>
