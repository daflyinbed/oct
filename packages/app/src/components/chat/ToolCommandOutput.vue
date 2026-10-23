<template>
  <div
    ref="liveEl"
    class="max-h-72 overflow-auto whitespace-pre-wrap break-words rounded-md bg-surface px-2.5 py-1.5 text-code text-neutral-10 font-mono"
    @scroll="onLiveScroll"
  >
    <span v-if="command.cwd" class="text-accent-10" :title="command.cwd">{{
      shortCwd(command.cwd)
    }}</span
    ><span class="text-faint">$ </span>{{ command.command
    }}<template
      v-if="
        part.status === 'running' ||
        (part.status === 'done' && part.content === null)
      "
      ><span
        v-for="(seg, i) in part.liveOutput.segments"
        :key="i"
        :class="seg.stream === 'stderr' ? 'text-warning-10' : 'text-neutral-10'"
        >{{ `\n${seg.text}` }}</span
      ></template
    ><span
      v-if="part.status === 'done' && part.content && !part.cleared"
      :class="part.isError ? 'text-danger-10' : ''"
      >{{ `\n${part.content}` }}</span
    >
  </div>
</template>

<script setup lang="ts">
import { computed, nextTick, ref, watch } from "vue";
import type { ToolCommand } from "./toolCardArguments";
import type { ToolCallPart } from "@/composables/chat/display";

const props = defineProps<{ part: ToolCallPart; command: ToolCommand }>();
// 提示符里的 cwd 过长时只留尾部一段，完整路径由 hover（title）展示
const CWD_TAIL_CHARS = 28;

function shortCwd(cwd: string): string {
  return cwd.length > CWD_TAIL_CHARS ? `…${cwd.slice(-CWD_TAIL_CHARS)}` : cwd;
}

// --- 实时输出自动滚动（仅在用户本就位于底部时跟随） ---------------------------

const liveEl = ref<HTMLElement | null>(null);
const stickToBottom = defineModel<boolean>("following", { required: true });

// 活跃输出总字符数（含被裁掉的头部），作为跟随滚动的触发信号
const liveSize = computed(() => {
  let size = props.part.liveOutput.droppedChars;
  for (const seg of props.part.liveOutput.segments) size += seg.text.length;
  return size;
});

function onLiveScroll() {
  const el = liveEl.value;
  if (!el) return;
  stickToBottom.value = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
}

watch(liveSize, async () => {
  if (!stickToBottom.value) return;
  await nextTick();
  const el = liveEl.value;
  if (el) el.scrollTop = el.scrollHeight;
});
</script>
