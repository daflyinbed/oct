<template>
  <div class="py-1 text-[11.5px] text-neutral-10 leading-[1.7] font-mono">
    <div
      v-for="(line, idx) in lines"
      :key="idx"
      class="whitespace-pre-wrap break-words py-px pl-[calc(0.75rem+2.75rem)] pr-3 indent-[-2.75rem]"
      :class="
        line.type === 'add'
          ? 'bg-success-1 text-success-10'
          : line.type === 'remove'
            ? 'bg-danger-1 text-danger-10'
            : ''
      "
    >
      <span class="mr-3 inline-block w-8 select-none text-right text-faint">{{
        line.num
      }}</span
      >{{ line.content }}
    </div>
  </div>
</template>

<script setup lang="ts">
interface DiffLine {
  num: number;
  type: string;
  content: string;
}

defineProps<{
  lines: DiffLine[];
}>();
</script>
