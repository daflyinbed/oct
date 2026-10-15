import { beforeEach, describe, expect, it, vi } from "vitest";
import client from "@/api/client";
import { useProviders } from "./useProviders";
import type { components } from "@/api/schema";

vi.mock("@/api/client", () => ({ default: { GET: vi.fn() } }));

type Provider = components["schemas"]["ProviderResponse"];
const store = useProviders();
const GET = vi.mocked(client.GET);

async function loadOptions(
  reasoningOptions: unknown,
  adapterType = "anthropic",
) {
  const provider: Provider = {
    id: "reasoning-provider",
    name: "Reasoning provider",
    adapter_type: adapterType,
    api_key_set: true,
    source: "custom",
    created_at: "2026-10-15T00:00:00",
    updated_at: "2026-10-15T00:00:00",
    models: [
      {
        model_id: "model",
        name: "Model",
        reasoning: true,
        reasoning_options: reasoningOptions,
        tool_call: true,
        attachment: false,
        is_enabled: true,
        source: "custom",
        limit_output: 8192,
      },
    ],
  };
  GET.mockResolvedValue({ data: [provider] } as never);
  await store.fetchProviders();
  store.selectProvider(provider.id);
}

beforeEach(() => vi.clearAllMocks());

describe("reasoning controls", () => {
  it("reads catalog arrays with multiple controls and preserves every effort", async () => {
    await loadOptions([
      { type: "effort", values: ["low", "high", "max", null] },
      { type: "budget_tokens", min: 1024, max: 31999 },
      { type: "toggle" },
    ]);
    const values = store.reasoningOptions.value.map((option) => option.value);
    expect(values).toContain("effort:max");
    expect(values).toContain("on");
    const budgets = values.filter((value) => value.startsWith("budget:"));
    expect(budgets.length).toBe(3);
    expect(budgets).toContain("budget:8191");
    store.selectReasoning("effort:max");
    expect(store.getReasoningRequest()).toEqual({
      type: "effort",
      effort: "max",
    });
    store.selectReasoning("budget:8191");
    expect(store.getReasoningRequest()).toEqual({
      type: "budget_tokens",
      budget: 8191,
    });
    store.selectReasoning("default");
    expect(store.getReasoningRequest()).toBeNull();
  });

  it("hides unsupported OpenAI-compatible controls and refuses stale selections", async () => {
    await loadOptions(
      [
        { type: "toggle" },
        { type: "budget_tokens", min: 1024 },
        { type: "effort", values: ["high", "max"] },
      ],
      "openai_compatible",
    );
    expect(store.reasoningOptions.value.map((option) => option.value)).toEqual([
      "effort:high",
      "effort:max",
    ]);
    store.selectReasoning("on");
    expect(store.getReasoningRequest()).toBeNull();
    store.selectReasoning("effort:max");
    store.selectModel("model");
    expect(store.getReasoningRequest()).toBeNull();
  });

  it.each([{ raw: [] }, { raw: null }, { raw: [{ type: "future" }] }])(
    "does not invent levels for $raw",
    async ({ raw }) => {
      await loadOptions(raw);
      expect(store.reasoningOptions.value).toEqual([]);
    },
  );

  it("keeps legacy custom single-control objects usable", async () => {
    await loadOptions({ type: "effort", values: ["high"] });
    expect(store.reasoningOptions.value).toEqual([
      { value: "effort:high", label: "high" },
    ]);
  });
});

describe("hydrateLastRequest（上一笔请求水合）", () => {
  const providerId = "reasoning-provider";

  /** 三模型目录：model 带档位，model-b 无档位，model-c 已停用。 */
  async function loadCatalog() {
    const base = {
      tool_call: true,
      attachment: false,
      is_enabled: true,
      source: "custom",
      limit_output: 8192,
    };
    const provider: Provider = {
      id: providerId,
      name: "Reasoning provider",
      adapter_type: "anthropic",
      api_key_set: true,
      source: "custom",
      created_at: "2026-10-15T00:00:00",
      updated_at: "2026-10-15T00:00:00",
      models: [
        {
          ...base,
          model_id: "model",
          name: "Model",
          reasoning: true,
          reasoning_options: [
            { type: "effort", values: ["low", "high"] },
            { type: "toggle" },
            { type: "budget_tokens", min: 1024, max: 8191 },
          ],
        },
        {
          ...base,
          model_id: "model-b",
          name: "Model B",
          reasoning: false,
          reasoning_options: null,
        },
        {
          ...base,
          model_id: "model-c",
          name: "Model C",
          reasoning: false,
          is_enabled: false,
        },
      ],
    };
    GET.mockResolvedValue({ data: [provider] } as never);
    await store.fetchProviders();
    store.selectProvider(providerId);
  }

  it("恢复上一笔请求的模型与 effort 档位", async () => {
    await loadCatalog();
    store.hydrateLastRequest({
      providerId,
      modelId: "model",
      reasoning: { type: "effort", effort: "high" },
    });
    expect(store.selectedProviderId.value).toBe(providerId);
    expect(store.selectedModelId.value).toBe("model");
    expect(store.selectedReasoning.value).toBe("effort:high");
    expect(store.getReasoningRequest()).toEqual({
      type: "effort",
      effort: "high",
    });
  });

  it("恢复 budget 与 toggle 档位（限当前预设档）", async () => {
    await loadCatalog();
    store.hydrateLastRequest({
      providerId,
      modelId: "model",
      reasoning: { type: "budget_tokens", budget: 4608 },
    });
    expect(store.selectedReasoning.value).toBe("budget:4608");
    store.hydrateLastRequest({
      providerId,
      modelId: "model",
      reasoning: { type: "enabled" },
    });
    expect(store.selectedReasoning.value).toBe("on");
  });

  it("档位不在当前值域（非预设 budget / 目录已删的 effort）时落回「默认」", async () => {
    await loadCatalog();
    store.hydrateLastRequest({
      providerId,
      modelId: "model",
      reasoning: { type: "effort", effort: "ultra" },
    });
    expect(store.selectedReasoning.value).toBeNull();
    store.hydrateLastRequest({
      providerId,
      modelId: "model",
      reasoning: { type: "budget_tokens", budget: 2048 },
    });
    expect(store.selectedReasoning.value).toBeNull();
    store.hydrateLastRequest({ providerId, modelId: "model", reasoning: null });
    expect(store.selectedReasoning.value).toBeNull();
  });

  it("目录里已消失/停用的模型不动当前选择，档位随之失效", async () => {
    await loadCatalog();
    store.hydrateLastRequest({
      providerId: "gone",
      modelId: "x",
      reasoning: { type: "effort", effort: "high" },
    });
    expect(store.selectedModelId.value).toBe("model");
    expect(store.selectedReasoning.value).toBeNull();
    store.hydrateLastRequest({
      providerId,
      modelId: "model-c",
      reasoning: { type: "effort", effort: "high" },
    });
    expect(store.selectedModelId.value).toBe("model");
    expect(store.selectedReasoning.value).toBeNull();
  });

  it.each([
    { raw: { type: "effort" } },
    { raw: "garbage" },
    { raw: { type: "future", values: ["x"] } },
    { raw: { type: "budget_tokens", budget: -1 } },
  ])("损坏的落库 reasoning 按无档位处理", async ({ raw }) => {
    await loadCatalog();
    store.hydrateLastRequest({ providerId, modelId: "model", reasoning: raw });
    expect(store.selectedReasoning.value).toBeNull();
  });

  it("null 输入（新会话无历史请求）不动选择", async () => {
    await loadCatalog();
    store.selectReasoning("effort:high");
    store.hydrateLastRequest(null);
    expect(store.selectedReasoning.value).toBe("effort:high");
  });
});

describe("whenProvidersReady（目录竞态兜底）", () => {
  it("等待进行中的目录请求落定后再返回", async () => {
    let resolveGet!: (value: unknown) => void;
    GET.mockImplementation(
      () => new Promise((resolve) => (resolveGet = resolve)) as never,
    );
    const pending = store.fetchProviders();
    const ready = store.whenProvidersReady();
    let settled = false;
    void ready.then(() => {
      settled = true;
    });
    expect(settled).toBe(false);
    resolveGet({ data: [] });
    await Promise.all([pending, ready]);
    expect(settled).toBe(true);
  });
});
