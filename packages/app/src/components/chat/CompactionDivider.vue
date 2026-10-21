<template>
  <div class="border border-line-soft rounded-[10px] bg-panel">
    <button
      type="button"
      class="w-full flex items-center gap-2 rounded-[10px] px-3 py-2 text-left control-focus"
      :title="open ? '收起摘要' : '展开摘要'"
      @click="open = !open"
    >
      <i-lucide-archive class="h-3.5 w-3.5 flex-none text-faint" />
      <span class="min-w-0 flex-1 truncate text-sm text-dim">
        已压缩 {{ part.compactedMessages }} 条较早消息{{ savedText }}
      </span>
      <span class="flex flex-none items-center gap-1 text-xs text-faint">
        {{ part.triggeredBy === "manual" ? "手动" : "自动" }}
        <i-lucide-chevron-down
          class="h-3 w-3 transition-transform"
          :class="open ? 'rotate-180' : ''"
        />
      </span>
    </button>
    <div v-if="open" class="border-t border-line-soft px-3 py-2">
      <pre
        class="max-h-[280px] overflow-auto whitespace-pre-wrap break-words text-base text-dim leading-[1.5] font-sans"
        >{{ part.summary }}</pre>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed, ref } from "vue";
import { formatTokenCount } from "@/composables/useContextUsage";
import type { CompactionPart } from "@/composables/useChat";

const props = defineProps<{
  part: CompactionPart;
}>();

const open = ref(false);

const savedText = computed(() => {
  const { estTokensBefore: before, estTokensAfter: after } = props.part;
  if (before == null || after == null || before <= after) return "";
  return ` · 腾出约 ${formatTokenCount(before - after)} tokens`;
});
</script>
