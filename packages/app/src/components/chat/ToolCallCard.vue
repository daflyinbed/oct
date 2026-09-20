<template>
  <div
    class="min-w-0 border rounded-[10px] bg-panel"
    :class="part.isError ? 'border-danger-6' : 'border-line-soft'"
  >
    <!-- 头部：工具图标 + 名称 + 标题 + 状态（生成中脉冲 / 运行中 spinner / 完成后徽标） -->
    <div class="min-w-0 flex items-center gap-2 px-3 py-1.5">
      <span
        class="h-3.5 w-3.5 flex-none"
        :class="
          part.status === 'done' && part.isError ? 'text-danger-10' : 'text-dim'
        "
      >
        <span
          v-if="part.status === 'done'"
          class="block h-full w-full"
          :class="part.isError ? 'i-lucide-circle-x' : 'i-lucide-circle-check'"
        />
        <span v-else class="block h-full w-full" :class="iconClass" />
      </span>

      <span
        v-if="part.name"
        class="max-w-[40%] flex-none truncate text-[12px] text-strong font-mono"
        :title="part.name"
        >{{ part.name }}</span
      >
      <span
        v-else
        class="h-3 w-16 flex-none animate-pulse rounded bg-neutral-3"
      />

      <span
        v-if="titleText"
        class="min-w-0 flex-1 truncate text-[12px] text-faint"
        :title="titleText"
      >
        {{ titleText }}
      </span>
      <span v-else class="flex-1" />

      <span class="flex flex-none items-center gap-1">
        <template v-if="part.status === 'generating'">
          <span
            class="block h-1.5 w-1.5 animate-pulse rounded-full bg-accent-7"
          />
        </template>
        <i-lucide-loader-circle
          v-else-if="part.status === 'running'"
          class="h-3.5 w-3.5 animate-spin text-accent-7"
        />
        <template v-else>
          <span
            v-for="chip in chips"
            :key="chip.key"
            class="h-[16px] inline-flex items-center whitespace-nowrap rounded-full px-1.5 text-[10.5px] font-medium leading-none"
            :class="toneClass[chip.tone]"
            >{{ chip.label }}</span
          >
        </template>
      </span>
    </div>

    <!-- 生成中：参数流式累积， pulsing cursor，不解析半截 JSON -->
    <div
      v-if="part.status === 'generating' && part.arguments"
      class="mx-3 mb-2 max-h-40 overflow-auto whitespace-pre-wrap break-all rounded-md bg-surface px-2.5 py-1.5 text-[11.5px] text-dim leading-[1.5] font-mono"
    >
      {{ part.arguments }}<span class="animate-pulse text-accent-7">▌</span>
    </div>

    <!-- 运行中：canonical 参数 -->
    <div
      v-else-if="part.status === 'running' && part.arguments"
      class="mx-3 mb-2 max-h-40 overflow-auto whitespace-pre-wrap break-all rounded-md bg-surface px-2.5 py-1.5 text-[11.5px] text-dim leading-[1.5] font-mono"
    >
      {{ prettyArguments }}
    </div>

    <template v-if="part.status !== 'generating'">
      <!-- 实时输出：stdout 正常 / stderr 告警色，按到达顺序分段渲染；
           用户在底部时才自动跟随滚动 -->
      <div
        v-if="hasLiveOutput"
        ref="liveEl"
        class="mx-3 mb-2 max-h-56 overflow-auto whitespace-pre-wrap break-all rounded-md bg-surface px-2.5 py-1.5 text-[11.5px] leading-[1.5] font-mono"
        @scroll="onLiveScroll"
      >
        <span v-if="part.liveOutput.droppedChars > 0" class="text-faint">{{
          `…（已省略最早的 ${part.liveOutput.droppedChars} 字符）\n`
        }}</span>
        <span
          v-for="(seg, i) in part.liveOutput.segments"
          :key="i"
          :class="
            seg.stream === 'stderr' ? 'text-warning-10' : 'text-neutral-10'
          "
          >{{ seg.text }}</span
        >
      </div>

      <template v-if="part.status === 'done'">
        <!-- 调用参数（默认折叠） -->
        <CollapsibleRoot
          class="border-t border-line-soft"
          :unmount-on-hide="true"
        >
          <CollapsibleTrigger
            class="group w-full flex items-center gap-1.5 px-3 py-1.5 text-left text-[11.5px] text-dim control-focus transition-colors hover:text-neutral-10"
          >
            <i-lucide-chevron-right
              class="h-3 w-3 flex-none transition-transform group-data-[state=open]:rotate-90"
            />
            调用参数
          </CollapsibleTrigger>
          <CollapsibleContent>
            <pre
              class="mx-3 mb-2 max-h-72 overflow-auto whitespace-pre-wrap break-all rounded-md bg-surface px-2.5 py-1.5 text-[11.5px] text-neutral-10 leading-[1.55] font-mono"
              >{{ prettyArguments }}</pre
            >
          </CollapsibleContent>
        </CollapsibleRoot>

        <!-- 返回给模型（默认展开） -->
        <CollapsibleRoot
          v-if="part.content !== null"
          class="border-t border-line-soft"
          :default-open="true"
        >
          <CollapsibleTrigger
            class="group w-full flex items-center gap-1.5 px-3 py-1.5 text-left text-[11.5px] text-dim control-focus transition-colors hover:text-neutral-10"
          >
            <i-lucide-chevron-right
              class="h-3 w-3 flex-none transition-transform group-data-[state=open]:rotate-90"
            />
            返回给模型
          </CollapsibleTrigger>
          <CollapsibleContent>
            <pre
              class="mx-3 mb-2 max-h-72 overflow-auto whitespace-pre-wrap break-all rounded-md px-2.5 py-1.5 text-[11.5px] leading-[1.55] font-mono"
              :class="
                part.isError
                  ? 'bg-danger-1 text-danger-10'
                  : 'bg-surface text-neutral-10'
              "
              >{{ part.content }}</pre
            >
            <p v-if="isTruncated" class="px-3 pb-2 text-[11px] text-faint">
              此内容已返回给模型（含截断）
            </p>
          </CollapsibleContent>
        </CollapsibleRoot>
      </template>
    </template>
  </div>
</template>

<script setup lang="ts">
import {
  CollapsibleContent,
  CollapsibleRoot,
  CollapsibleTrigger,
} from "reka-ui";
import { computed, nextTick, ref, watch } from "vue";
import type { ToolCallPart } from "@/composables/useChat";

const props = defineProps<{
  part: ToolCallPart;
}>();

// 图标按工具名映射（完整类名写在此处，UnoCSS 可从源码提取）
const iconClass = computed(() => {
  switch (props.part.name) {
    case "execute_command":
      return "i-lucide-terminal";
    case "read_file":
      return "i-lucide-file-text";
    case "write_file":
      return "i-lucide-file-pen";
    case "edit_file":
      return "i-lucide-file-pen-line";
    case "list_dir":
      return "i-lucide-folder";
    case "grep":
      return "i-lucide-text-search";
    case "glob":
      return "i-lucide-folder-search";
    default:
      return "i-lucide-wrench";
  }
});

const titleText = computed(() => props.part.title?.trim() || null);

// --- details 徽标 -----------------------------------------------------------

type ChipTone = "success" | "danger" | "warning" | "neutral";
interface Chip {
  key: string;
  label: string;
  tone: ChipTone;
}

const toneClass: Record<ChipTone, string> = {
  success: "bg-success-7 text-success-10",
  danger: "bg-danger-7 text-danger-10",
  warning: "bg-warning-7 text-warning-10",
  neutral: "bg-neutral-2 text-dim",
};

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  if (ms < 60000) return `${(ms / 1000).toFixed(1)}s`;
  return `${Math.floor(ms / 60000)}m ${Math.floor((ms % 60000) / 1000)}s`;
}

const chips = computed<Chip[]>(() => {
  const d = props.part.details;
  if (!d || props.part.status !== "done") return [];
  const num = (key: string): number | null =>
    typeof d[key] === "number" ? (d[key] as number) : null;
  const flag = (key: string) => d[key] === true;

  const result: Chip[] = [];

  const shown = num("shown_lines");
  const total = num("total_lines");
  if (shown !== null && total !== null) {
    result.push({
      key: "lines",
      label: `${shown}/${total} 行`,
      tone: "neutral",
    });
  }
  const matched = num("matched");
  if (matched !== null) {
    result.push({
      key: "matched",
      label: `${matched} 个匹配`,
      tone: "neutral",
    });
  }
  const found = num("found");
  if (found !== null) {
    result.push({ key: "found", label: `${found} 个结果`, tone: "neutral" });
  }
  const bytes = num("bytes");
  if (bytes !== null) {
    result.push({ key: "bytes", label: formatBytes(bytes), tone: "neutral" });
  }
  const replacements = num("replacements");
  if (replacements !== null) {
    result.push({
      key: "replacements",
      label: `${replacements} 处替换`,
      tone: "neutral",
    });
  }

  const exitCode = num("exit_code");
  if (exitCode !== null) {
    result.push({
      key: "exit",
      label: `exit ${exitCode}`,
      tone: exitCode === 0 ? "success" : "danger",
    });
  }
  if (flag("timed_out")) {
    result.push({ key: "timed_out", label: "超时", tone: "warning" });
  }
  if (flag("created")) {
    result.push({ key: "created", label: "新建文件", tone: "success" });
  }
  const wallTime = num("wall_time_ms");
  if (wallTime !== null) {
    result.push({
      key: "wall",
      label: formatDuration(wallTime),
      tone: "neutral",
    });
  }
  if (flag("truncated")) {
    const original = num("original_bytes");
    result.push({
      key: "truncated",
      label:
        original !== null ? `已截断 · 原始 ${formatBytes(original)}` : "已截断",
      tone: "warning",
    });
  }
  return result;
});

const isTruncated = computed(() => props.part.details?.truncated === true);

// --- 参数展示 ---------------------------------------------------------------

const prettyArguments = computed(() => {
  const raw = props.part.arguments;
  if (!raw) return "";
  try {
    return JSON.stringify(JSON.parse(raw), null, 2);
  } catch {
    return raw;
  }
});

// --- 实时输出自动滚动（仅在用户本就位于底部时跟随） ---------------------------

const liveEl = ref<HTMLElement | null>(null);
const stickToBottom = ref(true);

const hasLiveOutput = computed(() => props.part.liveOutput.segments.length > 0);

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
