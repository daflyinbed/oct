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
