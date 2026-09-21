<template>
  <!-- 思考卡片：流式期间默认展开跟随，结束后折叠为单行摘要，点击可再展开 -->
  <div class="min-w-0">
    <button
      type="button"
      class="group min-w-0 w-full flex items-center gap-2 rounded-md px-1.5 py-1 text-left control-focus transition-colors hover:bg-neutral-2"
      :aria-expanded="open"
      @click="open = !open"
    >
      <span class="i-lucide-lightbulb block h-3.5 w-3.5 flex-none text-dim" />
      <span class="flex-none text-[12px] text-strong">思考</span>
      <span v-if="durationText" class="flex-none text-[12px] text-faint"
        >· {{ durationText }}</span
      >
      <span class="flex-1" />
      <span
        v-if="!part.ended"
        class="block h-1.5 w-1.5 flex-none animate-pulse rounded-full bg-accent-7"
      />
      <i-lucide-chevron-right
        class="h-3.5 w-3.5 flex-none text-faint transition-all duration-150"
        :class="
          open
            ? 'rotate-90 opacity-100'
            : 'opacity-0 group-hover:opacity-100 group-focus-visible:opacity-100'
        "
      />
    </button>

    <!-- 展开后的思考正文：左侧细线引用样式，弱化展示 -->
    <div v-if="open" class="mb-1 ml-1.5 mt-0.5">
      <div
        class="max-h-72 overflow-auto whitespace-pre-wrap break-words border-l border-line-soft py-0.5 pl-3 text-[12.5px] text-dim leading-[1.6]"
      >
        {{ part.text
        }}<span v-if="!part.ended" class="animate-pulse text-accent-7">▌</span>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed, ref, watch } from "vue";
import type { ReasoningPart } from "@/composables/useChat";

const props = defineProps<{
  part: ReasoningPart;
}>();

// 流式期间强制展开；结束的那一刻自动折叠（时长同时落定），之后用户可开合
const open = ref(!props.part.ended);
watch(
  () => props.part.ended,
  (ended) => {
    if (ended) open.value = false;
  },
);

// 短思考一律显示模糊的「几秒」，超过 10 秒后给出精确值；未知（历史无时长）不显示
const durationText = computed<string | null>(() => {
  const ms = props.part.durationMs;
  if (ms === null) return null;
  const s = ms / 1000;
  if (s < 10) return "持续了几秒";
  if (s < 60) return `持续了 ${Math.round(s)} 秒`;
  return `持续了 ${Math.floor(s / 60)} 分 ${Math.round(s % 60)} 秒`;
});
</script>
