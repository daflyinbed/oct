import { beforeEach, describe, expect, it } from "vitest";
import {
  clearContextUsage,
  contextUsageLevel,
  contextUsagePercent,
  formatTokenCount,
  recordUsageEvent,
  seedContextUsage,
  setCompacting,
  useContextUsage,
} from "@/composables/useContextUsage";
import type { components } from "@/api/schema";

type StoredMessage = components["schemas"]["StoredMessage"];

/** 后端 StoredMessage 的最小形状（仅用到本 composable 读的字段）。 */
function storedMessage(
  role: string,
  extra: Partial<StoredMessage> = {},
): StoredMessage {
  return {
    id: `msg-${Math.random().toString(36).slice(2)}`,
    conversation_id: "conv",
    role,
    parts_json: "[]",
    details_json: null,
    ordering: 0,
    provider_id: null,
    model_id: null,
    input_tokens: null,
    output_tokens: null,
    reasoning_tokens: null,
    created_at: "2026-09-20T00:00:00",
    ...extra,
  };
}

beforeEach(() => {
  clearContextUsage();
});

describe("contextUsagePercent", () => {
  it("窗口未知或无数据时为 null", () => {
    expect(contextUsagePercent(null, 200_000)).toBeNull();
    expect(contextUsagePercent(1000, null)).toBeNull();
    expect(contextUsagePercent(1000, 0)).toBeNull();
  });

  it("计算百分比并封顶 100%", () => {
    expect(contextUsagePercent(100_000, 200_000)).toBe(50);
    expect(contextUsagePercent(190_000, 200_000)).toBe(95);
    expect(contextUsagePercent(999_999, 200_000)).toBe(100);
  });
});

describe("contextUsageLevel", () => {
  it("阈值与后端自动压缩对齐：<60 ok / 60–85 warn / ≥85 danger", () => {
    expect(contextUsageLevel(null)).toBe("ok");
    expect(contextUsageLevel(59)).toBe("ok");
    expect(contextUsageLevel(60)).toBe("warn");
    expect(contextUsageLevel(84)).toBe("warn");
    expect(contextUsageLevel(85)).toBe("danger");
  });
});

describe("formatTokenCount", () => {
  it("千/万位缩写并去掉多余的 .0", () => {
    expect(formatTokenCount(842)).toBe("842");
    expect(formatTokenCount(1000)).toBe("1k");
    expect(formatTokenCount(84_200)).toBe("84.2k");
    expect(formatTokenCount(1_250_000)).toBe("1.3M");
  });
});

describe("useContextUsage 状态流", () => {
  it("usage 事件记录 input 与窗口", () => {
    const { inputTokens, windowTokens } = useContextUsage();
    recordUsageEvent({ input_tokens: 1200, context_window_tokens: 200_000 });
    expect(inputTokens.value).toBe(1200);
    expect(windowTokens.value).toBe(200_000);
  });

  it("播种取最后一条带 usage 的 assistant 行", () => {
    const { inputTokens } = useContextUsage();
    seedContextUsage([
      storedMessage("assistant", { input_tokens: 500 }),
      storedMessage("assistant"), // 无 usage 的行跳过
      storedMessage("assistant", { input_tokens: 900 }),
    ]);
    expect(inputTokens.value).toBe(900);
  });

  it("播种先清空旧值（切会话不串显）", () => {
    const { inputTokens } = useContextUsage();
    recordUsageEvent({ input_tokens: 1200 });
    seedContextUsage([storedMessage("assistant")]);
    expect(inputTokens.value).toBeNull();
  });

  it("compacting 状态由事件驱动", () => {
    const { compacting } = useContextUsage();
    setCompacting(true);
    expect(compacting.value).toBe(true);
    setCompacting(false);
    expect(compacting.value).toBe(false);
  });
});
