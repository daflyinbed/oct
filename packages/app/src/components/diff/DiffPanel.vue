<template>
  <aside v-if="visible" class="workspace-panel h-full w-full bg-panel">
    <header
      class="workspace-panel-header justify-between border-b border-line-soft select-none"
    >
      <div
        class="flex items-center gap-[7px] text-[12.5px] font-semibold text-neutral-10"
      >
        <i-lucide-git-compare class="w-3.5 h-3.5 text-dim" />
        Changes
      </div>
      <button
        class="control-ghost control-focus w-[22px] h-[22px] rounded-md text-[12px]"
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
