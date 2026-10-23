import { computed, readonly, ref, watch } from "vue";
import { useProviders } from "./useProviders";
import type { components } from "@/api/schema";

type ApiMessage = components["schemas"]["MessageDto"];

/**
 * 上下文占用状态（模块级单例，useMcpStatus 同款模式）：SSE usage
 * 事件更新真实用量与窗口，会话加载时从 stored assistant 行播种用量；
 * 尚无实时窗口时读取当前模型目录的 limit_context。供状态栏指示器消费。
 */
const inputTokens = ref<number | null>(null);
const reportedWindowTokens = ref<number | null>(null);
const compacting = ref(false);
const { providers, selectedProviderId, selectedModelId, getProviderSpec } =
  useProviders();

const windowTokens = computed(() => {
  if (reportedWindowTokens.value != null) return reportedWindowTokens.value;
  const provider = providers.value.find(
    (p) => p.id === selectedProviderId.value,
  );
  const window = provider?.models.find(
    (m) => m.model_id === selectedModelId.value,
  )?.limit_context;
  return window != null && window > 0 ? window : null;
});

// 实时窗口只适用于当前模型；目录首次就绪前收到的事件仍然有效。
watch(
  getProviderSpec,
  (_current, previous) => {
    if (previous != null) reportedWindowTokens.value = null;
  },
  { flush: "sync" },
);

/** SSE usage 事件落地：input_tokens 是 provider 报告的真实请求体积。 */
export function recordUsageEvent(data: {
  input_tokens?: number | null;
  context_window_tokens?: number | null;
}): void {
  if (data.input_tokens != null) inputTokens.value = data.input_tokens;
  if (data.context_window_tokens != null) {
    reportedWindowTokens.value = data.context_window_tokens;
  }
}

/**
 * 会话加载时播种：取最后一条带 usage 的 assistant 行（该行的
 * input_tokens 就是最近一次请求的真实上下文体积）。切换会话时
 * inputTokens 先清空，避免上一个会话的占用串显。
 */
export function seedContextUsage(stored: readonly ApiMessage[]): void {
  inputTokens.value = null;
  for (let i = stored.length - 1; i >= 0; i--) {
    const m = stored[i];
    if (m.role === "assistant" && m.input_tokens != null) {
      inputTokens.value = m.input_tokens;
      break;
    }
  }
}

/** 离开会话（空态）时清空用量和实时窗口；模型窗口仍由当前选择决定。 */
export function clearContextUsage(): void {
  inputTokens.value = null;
  reportedWindowTokens.value = null;
  compacting.value = false;
}

/** compacting / compacted 事件驱动；手动压缩请求前后同样置位。 */
export function setCompacting(value: boolean): void {
  compacting.value = value;
}

export function useContextUsage() {
  return {
    inputTokens: readonly(inputTokens),
    windowTokens: readonly(windowTokens),
    compacting: readonly(compacting),
  };
}

// ---------------------------------------------------------------------------
// 指示器格式化（纯函数，组件与测试共用）
// ---------------------------------------------------------------------------

/** 84200 → "84.2k"，1250000 → "1.3M"。 */
export function formatTokenCount(n: number): string {
  if (n >= 1_000_000) {
    return `${trimZero((n / 1_000_000).toFixed(1))}M`;
  }
  if (n >= 1_000) {
    return `${trimZero((n / 1_000).toFixed(1))}k`;
  }
  return `${n}`;
}

function trimZero(s: string): string {
  return s.endsWith(".0") ? s.slice(0, -2) : s;
}

/** 窗口未知或无数据时为 null（指示器退化为只显示绝对值或不显示百分比）。 */
export function contextUsagePercent(
  input: number | null,
  window: number | null,
): number | null {
  if (input == null || window == null || window <= 0) return null;
  return Math.min(100, Math.round((input / window) * 100));
}

/** <60% 正常 / 60–85% 偏高 / ≥85% 危险（与后端自动压缩阈值一致）。 */
export function contextUsageLevel(
  percent: number | null,
): "ok" | "warn" | "danger" {
  if (percent == null) return "ok";
  if (percent >= 85) return "danger";
  if (percent >= 60) return "warn";
  return "ok";
}
