<template>
  <div class="space-y-3">
    <div
      v-for="file in diffFiles"
      :key="file.path"
      class="border border-line-soft rounded-md overflow-hidden"
    >
      <div
        class="px-3 py-1.5 text-[11.5px] font-medium border-b border-line-soft bg-surface text-neutral-10 font-mono"
      >
        {{ file.path }}
      </div>
      <div class="py-1 text-[11.5px] font-mono leading-[1.7] text-neutral-10">
        <div
          v-for="(line, idx) in file.lines"
          :key="idx"
          class="px-3 py-px"
          :class="
            line.type === 'add'
              ? 'bg-success-1 text-success-10'
              : line.type === 'remove'
                ? 'bg-danger-1 text-danger-10'
                : ''
          "
        >
          <span
            class="inline-block w-8 text-right mr-3 text-faint select-none"
            >{{ line.num }}</span
          >
          {{ line.content }}
        </div>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
defineProps<{
  diffFiles: {
    path: string;
    lines: { num: number; type: string; content: string }[];
  }[];
}>();
</script>
