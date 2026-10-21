<template>
  <!-- 思考卡片默认折叠；流式内容在头行内横向跟随，点击可展开完整正文 -->
  <div class="min-w-0">
    <button
      type="button"
      class="group min-w-0 w-full flex items-center gap-2 rounded-md px-1.5 py-1 text-left control-focus transition-colors hover:bg-neutral-2"
      :aria-expanded="open"
      @click="open = !open"
    >
      <i-lucide-lightbulb class="block h-3.5 w-3.5 flex-none text-dim" />
      <span class="flex-none text-sm text-strong">思考</span>
      <span v-if="durationText" class="flex-none text-sm text-faint"
        >· {{ durationText }}</span
      >
      <span
        v-if="!open && !part.ended && part.text"
        ref="streamPreview"
        data-reasoning-preview
        data-testid="reasoning-preview"
        class="reasoning-stream-preview min-w-0 flex-1 overflow-x-auto whitespace-nowrap text-sm text-dim"
      >
        {{ part.text }}<span class="animate-pulse text-accent-7">▌</span>
      </span>
      <span v-else class="flex-1" />
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
    <div v-if="open" data-reasoning-body class="mb-1 ml-1.5 mt-0.5">
      <div
        class="max-h-72 overflow-auto whitespace-pre-wrap break-words border-l border-line-soft py-0.5 pl-3 text-base text-dim leading-[1.6]"
      >
        {{ part.text
        }}<span v-if="!part.ended" class="animate-pulse text-accent-7">▌</span>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed, nextTick, ref, watch } from "vue";
import type { ReasoningPart } from "@/composables/useChat";

const props = defineProps<{
  part: ReasoningPart;
}>();

const open = ref(false);
const streamPreview = ref<HTMLElement | null>(null);

// 折叠时始终跟随流式文本末尾；用户主动展开后保留其选择。
async function followStreamingText() {
  if (open.value || props.part.ended) return;
  await nextTick();
  if (streamPreview.value) {
    streamPreview.value.scrollLeft = streamPreview.value.scrollWidth;
  }
}

watch(() => props.part.text, followStreamingText, {
  flush: "post",
  immediate: true,
});
watch(open, followStreamingText, { flush: "post" });

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

<style scoped>
.reasoning-stream-preview {
  scrollbar-width: none;
}

.reasoning-stream-preview::-webkit-scrollbar {
  display: none;
}
</style>
