<template>
  <HoverCardRoot :open-delay="100">
    <HoverCardTrigger as-child>
      <button
        type="button"
        class="ml-1 h-6 flex items-center gap-1 rounded px-1.5 text-xs text-faint tabular-nums control-focus transition-colors hover:bg-neutral-2 hover:text-neutral-10"
        title="本轮用量"
      >
        <i-lucide-database class="h-3 w-3" />
        {{ total == null ? "用量详情" : `用量 ${formatTokenCount(total)} tok` }}
      </button>
    </HoverCardTrigger>
    <HoverCardPortal>
      <HoverCardContent
        side="top"
        align="start"
        :side-offset="8"
        class="z-50 w-[330px] max-w-[calc(100vw-24px)] border border-line-soft rounded-lg bg-surface p-3 text-xs shadow-[var(--oct-shadow)]"
      >
        <div
          class="mb-2 flex items-center justify-between gap-3 border-b border-line-soft pb-2 text-neutral-10"
        >
          <span class="flex items-center gap-1.5 font-medium"
            ><i-lucide-database class="h-3.5 w-3.5" />本轮用量</span
          >
          <span class="tabular-nums">{{ tokens(total) }}</span>
        </div>
        <dl class="space-y-1.5">
          <div
            v-for="(model, index) in models"
            :key="model"
            class="flex items-start justify-between gap-4"
          >
            <dt class="flex-none text-faint">
              {{ index === 0 ? "提供方 / 模型" : "" }}
            </dt>
            <dd class="min-w-0 break-all text-right text-dim">{{ model }}</dd>
          </div>
          <div class="flex justify-between gap-4">
            <dt class="text-faint">缓存命中</dt>
            <dd class="text-dim tabular-nums">{{ cacheHit }}</dd>
          </div>
          <div class="flex justify-between gap-4">
            <dt class="text-faint">
              {{ uncached == null ? "输入" : "未缓存输入" }}
            </dt>
            <dd class="text-dim tabular-nums">
              {{ tokens(uncached ?? input) }}
            </dd>
          </div>
          <div class="flex justify-between gap-4">
            <dt class="text-faint">缓存读取</dt>
            <dd class="text-dim tabular-nums">{{ tokens(cached) }}</dd>
          </div>
          <div
            v-if="created != null && created > 0"
            class="flex justify-between gap-4"
          >
            <dt class="text-faint">缓存写入</dt>
            <dd class="text-dim tabular-nums">{{ tokens(created) }}</dd>
          </div>
          <div class="flex justify-between gap-4">
            <dt class="text-faint">输出</dt>
            <dd class="text-dim tabular-nums">{{ tokens(output) }}</dd>
          </div>
          <div v-if="reasoning != null" class="flex justify-between gap-4">
            <dt class="text-faint">其中思考</dt>
            <dd class="text-dim tabular-nums">{{ tokens(reasoning) }}</dd>
          </div>
        </dl>
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
import { sumTokens } from "@/composables/chat/tokenUsage";
import { formatTokenCount } from "@/composables/useContextUsage";
import type { TokenUsage } from "@/composables/chat/tokenUsage";

const props = defineProps<{ usage: readonly TokenUsage[] }>();
const input = computed(() => sumTokens(props.usage, "inputTokens"));
const output = computed(() => sumTokens(props.usage, "outputTokens"));
const cached = computed(() => sumTokens(props.usage, "cachedInputTokens"));
const created = computed(() => sumTokens(props.usage, "cacheCreationTokens"));
const reasoning = computed(() => sumTokens(props.usage, "reasoningTokens"));
const total = computed(() =>
  input.value != null && output.value != null
    ? input.value + output.value
    : null,
);
const uncached = computed(() =>
  input.value != null && cached.value != null
    ? Math.max(0, input.value - cached.value - (created.value ?? 0))
    : null,
);
const cacheHit = computed(() =>
  input.value != null && input.value > 0 && cached.value != null
    ? `${((100 * cached.value) / input.value).toFixed(1)}%`
    : "—",
);
const models = computed(() => [
  ...new Set(
    props.usage.map(
      (item) => `${item.providerId ?? "未知"} / ${item.modelId ?? "未知"}`,
    ),
  ),
]);
function tokens(value: number | null): string {
  return value == null ? "—" : `${value.toLocaleString()} tok`;
}
</script>
