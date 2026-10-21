<template>
  <!-- 无数据（从未收发过消息）时整体隐藏 -->
  <HoverCardRoot v-if="inputTokens != null" :open-delay="100">
    <HoverCardTrigger as-child>
      <button
        class="h-[26px] w-[26px] flex flex-none items-center justify-center rounded-md control-focus transition-colors hover:bg-neutral-3"
        :class="levelTextClass"
        title="上下文容量"
      >
        <!-- 压缩进行中切 spinner；平时是环形进度（从 12 点方向顺时针） -->
        <i-lucide-loader-circle
          v-if="compacting"
          class="h-3.5 w-3.5 animate-spin"
        />
        <svg
          v-else
          class="h-3.5 w-3.5 -rotate-90"
          viewBox="0 0 16 16"
          aria-hidden="true"
        >
          <circle
            cx="8"
            cy="8"
            r="6"
            fill="none"
            stroke="currentColor"
            stroke-opacity="0.25"
            stroke-width="2"
          />
          <circle
            v-if="percent != null"
            cx="8"
            cy="8"
            r="6"
            fill="none"
            stroke="currentColor"
            stroke-width="2"
            stroke-linecap="round"
            class="transition-[stroke-dashoffset] duration-300"
            :stroke-dasharray="CIRCUMFERENCE"
            :stroke-dashoffset="dashOffset"
          />
        </svg>
      </button>
    </HoverCardTrigger>

    <HoverCardPortal>
      <HoverCardContent
        side="top"
        align="end"
        :side-offset="8"
        class="z-50 w-[290px] border border-neutral-5 rounded-lg bg-surface p-3 shadow-[var(--oct-shadow)]"
      >
        <div class="flex items-baseline justify-between gap-4">
          <span class="text-base text-neutral-10 font-medium">
            上下文容量
          </span>
          <span class="text-xs text-faint tabular-nums">
            {{ headerUsage }}
          </span>
        </div>

        <div
          v-if="percent != null"
          class="mt-2 h-1.5 overflow-hidden rounded-full bg-neutral-4"
        >
          <div
            class="h-full rounded-full transition-[width] duration-300"
            :class="levelBarClass"
            :style="{ width: `${percent}%` }"
          />
        </div>

        <div class="mt-2.5 text-sm space-y-1.5">
          <div class="flex items-center justify-between">
            <span class="text-dim">已用</span>
            <span class="text-neutral-10 tabular-nums">
              {{ formatTokenCount(inputTokens) }} tokens
            </span>
          </div>
          <div class="flex items-center justify-between">
            <span class="text-dim">窗口</span>
            <span class="text-neutral-10 tabular-nums">
              {{ windowText }}
            </span>
          </div>
          <div class="flex items-center justify-between">
            <span class="text-dim">自动压缩阈值</span>
            <span class="text-neutral-10 tabular-nums">
              85%{{ thresholdText }}
            </span>
          </div>
        </div>

        <p class="mt-2.5 border-t border-line-soft pt-2 text-xs text-faint">
          {{
            compacting
              ? "正在压缩上下文…"
              : "达到阈值自动压缩，也可发送 /compact 手动压缩"
          }}
        </p>
      </HoverCardContent>
    </HoverCardPortal>
  </HoverCardRoot>
</template>

<script setup lang="ts">
import {
  HoverCardContent,
  HoverCardPortal,
  HoverCardRoot,
  HoverCardTrigger,
} from "reka-ui";
import { computed } from "vue";
import {
  contextUsageLevel,
  contextUsagePercent,
  formatTokenCount,
  useContextUsage,
} from "@/composables/useContextUsage";

/**
 * 不传 props 时消费全局 useContextUsage 单例（主面板）；子代理 panel 传入
 * 自己实例的 usage 快照（props 覆盖，undefined 表示"用单例"）——子 usage
 * 不得写进单例（会覆盖父上下文环），只能经 props 喂给本组件。
 */
const props = defineProps<{
  inputTokens?: number | null;
  windowTokens?: number | null;
  compacting?: boolean;
}>();

// 与后端自动压缩阈值一致（agent/compact.rs 的窗口 ×0.85）
const AUTO_COMPACT_PERCENT = 85;

const RING_RADIUS = 6;
const CIRCUMFERENCE = 2 * Math.PI * RING_RADIUS;

const singletonUsage = useContextUsage();

const inputTokens = computed(() =>
  props.inputTokens !== undefined
    ? props.inputTokens
    : singletonUsage.inputTokens.value,
);
const windowTokens = computed(() =>
  props.windowTokens !== undefined
    ? props.windowTokens
    : singletonUsage.windowTokens.value,
);
const compacting = computed(() =>
  props.compacting !== undefined
    ? props.compacting
    : singletonUsage.compacting.value,
);

const percent = computed(() =>
  contextUsagePercent(inputTokens.value, windowTokens.value),
);
const level = computed(() => contextUsageLevel(percent.value));

const levelTextClass = computed(
  () =>
    ({
      ok: "text-success-10",
      warn: "text-warning-10",
      danger: "text-danger-10",
    })[level.value],
);
const levelBarClass = computed(
  () =>
    ({
      ok: "bg-success-10",
      warn: "bg-warning-10",
      danger: "bg-danger-10",
    })[level.value],
);

// 窗口未知时只有底轨，没有进度弧
const dashOffset = computed(() =>
  percent.value == null
    ? CIRCUMFERENCE
    : CIRCUMFERENCE * (1 - percent.value / 100),
);

const headerUsage = computed(() => {
  const input = inputTokens.value;
  if (input == null) return "";
  if (percent.value != null && windowTokens.value != null) {
    return `${formatTokenCount(input)} / ${formatTokenCount(windowTokens.value)}（${percent.value}%）`;
  }
  return formatTokenCount(input);
});

const windowText = computed(() =>
  windowTokens.value != null
    ? `${formatTokenCount(windowTokens.value)} tokens`
    : "未知",
);

const thresholdText = computed(() =>
  windowTokens.value != null
    ? `（${formatTokenCount(Math.floor((windowTokens.value * AUTO_COMPACT_PERCENT) / 100))}）`
    : "",
);
</script>
