import { readonly, ref } from "vue";
import type { components } from "@/api/schema";

type StoredMessage = components["schemas"]["StoredMessage"];

/**
 * 上下文占用状态（模块级单例，useMcpStatus 同款模式）：数据源两条——
 * SSE usage 事件（后端每轮推送，含窗口大小）与会话加载时从 stored
 * assistant 行的 input_tokens 播种。供状态栏指示器消费。
 */
const inputTokens = ref<number | null>(null);
const windowTokens = ref<number | null>(null);
const compacting = ref(false);

/** SSE usage 事件落地：input_tokens 是 provider 报告的真实请求体积。 */
export function recordUsageEvent(data: {
  input_tokens?: number | null;
  context_window_tokens?: number | null;
}): void {
  if (data.input_tokens != null) inputTokens.value = data.input_tokens;
  if (data.context_window_tokens != null) {
    windowTokens.value = data.context_window_tokens;
  }
}

/**
 * 会话加载时播种：取最后一条带 usage 的 assistant 行（该行的
 * input_tokens 就是最近一次请求的真实上下文体积）。切换会话时
 * inputTokens 先清空，避免上一个会话的占用串显。
 */
export function seedContextUsage(stored: readonly StoredMessage[]): void {
  inputTokens.value = null;
  for (let i = stored.length - 1; i >= 0; i--) {
    const m = stored[i];
    if (m.role === "assistant" && m.input_tokens != null) {
      inputTokens.value = m.input_tokens;
      break;
    }
  }
}

/** 离开会话（空态）时清空。 */
export function clearContextUsage(): void {
  inputTokens.value = null;
  windowTokens.value = null;
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
