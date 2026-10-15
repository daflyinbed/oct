import { computed, readonly, ref, shallowReadonly } from "vue";
import client from "@/api/client";
import type { components } from "@/api/schema";

type ProviderResponse = components["schemas"]["ProviderResponse"];
type ModelSummary = components["schemas"]["ModelSummary"];
type ReasoningRequestDto = components["schemas"]["ReasoningRequestDto"];

const providers = ref<ProviderResponse[]>([]);
const loading = ref(false);
const selectedProviderId = ref<string | null>(null);
const selectedModelId = ref<string | null>(null);
/** 思考档位选择（AppSelect 的 value；null = 不带档位，走模型默认）。 */
const selectedReasoning = ref<string | null>(null);

/** models.dev `reasoning_options` 的运行时形态（后端透传的 JSON）。 */
interface RawReasoningOptions {
  type?: unknown;
  values?: unknown;
  min?: unknown;
  max?: unknown;
}

function asRawReasoningOptions(value: unknown): RawReasoningOptions | null {
  if (typeof value !== "object" || value === null || Array.isArray(value))
    return null;
  return value as RawReasoningOptions;
}

/** token 数 → 「8k / 1M」风格标签。 */
function formatTokens(n: number): string {
  if (n >= 1_000_000 && n % 1_000_000 === 0) return `${n / 1_000_000}M`;
  if (n >= 1024 && n % 1024 === 0) return `${n / 1024}k`;
  return `${n}`;
}

export function useProviders() {
  /**
   * 当前模型的思考档位选项。解析 `reasoning_options`：
   * - effort → values 列表（原文档位名，直接透传给 wire）
   * - toggle → 单一「开」选项
   * - budget_tokens → min/中点/max 三个预设档（对齐 opencode 的两三档做法，
   *   不做自由输入）
   * - 目录是控制项数组，可同时支持多种控制；兼容旧自定义模型的单个对象。
   * - 未知/空配置不猜测档位；openai_compatible 只展示可发送的 effort。
   */
  const reasoningOptions = computed(() => {
    const provider = providers.value.find(
      (p) => p.id === selectedProviderId.value,
    );
    const model = provider?.models.find(
      (m) => m.model_id === selectedModelId.value,
    );
    if (!model || !model.reasoning) return [];
    const rawOptions = Array.isArray(model.reasoning_options)
      ? model.reasoning_options
      : [model.reasoning_options];
    const options = rawOptions.flatMap((value) => {
      const raw = asRawReasoningOptions(value);
      if (raw?.type === "effort" && Array.isArray(raw.values)) {
        const values = raw.values.filter(
          (v): v is string => typeof v === "string" && v.length > 0,
        );
        if (values.length > 0) {
          return values.map((v) => ({ value: `effort:${v}`, label: v }));
        }
      }
      if (provider?.adapter_type !== "anthropic") return [];
      if (raw?.type === "toggle") return [{ value: "on", label: "开" }];
      if (raw?.type === "budget_tokens") {
        const min =
          typeof raw.min === "number" ? Math.max(1024, raw.min) : 1024;
        const outputLimit = model.limit_output ?? 8192;
        const max = Math.min(
          typeof raw.max === "number" ? raw.max : min * 8,
          outputLimit - 1,
        );
        if (max < min) return [];
        const mid = Math.round((min + max) / 2);
        const budgets = [...new Set([min, mid, max])];
        return budgets.map((b) => ({
          value: `budget:${b}`,
          label: formatTokens(b),
        }));
      }
      return [];
    });
    return options.filter(
      (option, index) =>
        options.findIndex((other) => other.value === option.value) === index,
    );
  });

  /** 选中档位 → 请求体 JSON（`null` = 本次请求不带 reasoning 字段）。 */
  const getReasoningRequest = (): ReasoningRequestDto | null => {
    const sel = selectedReasoning.value;
    if (!sel || !reasoningOptions.value.some((option) => option.value === sel))
      return null;
    if (sel === "on") return { type: "enabled" };
    if (sel.startsWith("effort:")) {
      return { type: "effort", effort: sel.slice("effort:".length) };
    }
    if (sel.startsWith("budget:")) {
      const budget = Number(sel.slice("budget:".length));
      return Number.isFinite(budget) && budget > 0
        ? { type: "budget_tokens", budget }
        : null;
    }
    return null;
  };

  const selectReasoning = (value: string) => {
    // 非空标识 default 是控件里的「默认」项：回到不带档位。
    selectedReasoning.value = value === "default" ? null : value;
  };
  const fetchProviders = async () => {
    loading.value = true;
    try {
      const { data, error } = await client.GET("/api/providers");
      if (!error && data) {
        providers.value = data;
        if (data.length > 0 && !selectedProviderId.value) {
          const withKey = data.find((p) => p.api_key_set);
          if (withKey) {
            selectedProviderId.value = withKey.id;
            const enabled = withKey.models.filter((m) => m.is_enabled);
            if (enabled.length > 0) {
              selectedModelId.value = enabled[0].model_id;
            }
          }
        }
      }
    } catch {
      // 网络异常：保持既有列表（无独立的错误视图），loading 必须复位。
    } finally {
      loading.value = false;
    }
  };

  const updateApiKey = async (providerId: string, apiKey: string) => {
    const { error } = await client.PUT("/api/providers/{id}", {
      params: { path: { id: providerId } },
      body: { api_key: apiKey || null },
    });
    if (!error) {
      const idx = providers.value.findIndex((p) => p.id === providerId);
      if (idx !== -1) {
        providers.value = providers.value.map((p, i) =>
          i === idx ? { ...p, api_key_set: apiKey.length > 0 } : p,
        );
      }
    }
    return !error;
  };

  /** 按需取回明文 Key（详情页「显示」用）；未配置或请求失败返回 null。 */
  const getApiKey = async (providerId: string): Promise<string | null> => {
    const { data, error } = await client.GET("/api/providers/{id}/api-key", {
      params: { path: { id: providerId } },
    });
    return error || !data ? null : data.api_key;
  };

  /** 断开（清空 Key 使其退出已连接列表）：body 必须是显式空串，后端 null 语义是「保持不变」。 */
  const disconnectProvider = async (providerId: string) => {
    const { error } = await client.PUT("/api/providers/{id}", {
      params: { path: { id: providerId } },
      body: { api_key: "" },
    });
    if (!error) {
      providers.value = providers.value.map((p) =>
        p.id === providerId ? { ...p, api_key_set: false } : p,
      );
    }
    return !error;
  };

  /** 删除自定义 provider（后端仅允许 source=custom，seeded 目录行不可删）；models 随 FK cascade。 */
  const deleteProvider = async (providerId: string) => {
    const { error } = await client.DELETE("/api/providers/{id}", {
      params: { path: { id: providerId } },
    });
    if (!error) {
      providers.value = providers.value.filter((p) => p.id !== providerId);
    }
    return !error;
  };

  const toggleModelEnabled = async (
    providerId: string,
    modelId: string,
    enabled: boolean,
  ) => {
    const { error } = await client.PUT(
      "/api/providers/{providerId}/models/{modelId}",
      {
        params: { path: { providerId, modelId } },
        body: { is_enabled: enabled },
      },
    );
    if (!error) {
      const pIdx = providers.value.findIndex((p) => p.id === providerId);
      if (pIdx !== -1) {
        providers.value = providers.value.map((p, i) => {
          if (i !== pIdx) return p;
          return {
            ...p,
            models: p.models.map((m) =>
              m.model_id === modelId ? { ...m, is_enabled: enabled } : m,
            ),
          };
        });
      }
    }
    return !error;
  };

  const createProvider = async (input: {
    id: string;
    name: string;
    base_url: string;
    api_key: string;
  }) => {
    const { data, error } = await client.POST("/api/providers", {
      body: input,
    });
    if (!error && data) {
      providers.value = [...providers.value, data];
      return data;
    }
    return null;
  };

  const createModel = async (
    providerId: string,
    input: { model_id: string; name: string; limit_context?: number | null },
  ) => {
    const { error } = await client.POST("/api/providers/{id}/models", {
      params: { path: { id: providerId } },
      body: input,
    });
    if (!error) {
      await fetchProviders();
      return true;
    }
    return false;
  };

  const selectProvider = (providerId: string) => {
    selectedProviderId.value = providerId;
    // 换 provider/model 后档位值域不同：回到「默认」。
    selectedReasoning.value = null;
    const provider = providers.value.find((p) => p.id === providerId);
    if (provider) {
      const enabled = provider.models.filter((m) => m.is_enabled);
      selectedModelId.value = enabled.length > 0 ? enabled[0].model_id : null;
    } else {
      selectedModelId.value = null;
    }
  };

  const selectModel = (modelId: string) => {
    selectedModelId.value = modelId;
    selectedReasoning.value = null;
  };

  const getProviderSpec = (): string | null => {
    if (!selectedProviderId.value || !selectedModelId.value) return null;
    return `${selectedProviderId.value}:${selectedModelId.value}`;
  };

  const enabledModels = (providerId: string): ModelSummary[] => {
    const provider = providers.value.find((p) => p.id === providerId);
    if (!provider) return [];
    return provider.models.filter((m) => m.is_enabled);
  };

  return {
    providers: shallowReadonly(providers),
    loading: readonly(loading),
    selectedProviderId: readonly(selectedProviderId),
    selectedModelId: readonly(selectedModelId),
    reasoningOptions,
    selectedReasoning: readonly(selectedReasoning),
    fetchProviders,
    updateApiKey,
    getApiKey,
    disconnectProvider,
    deleteProvider,
    toggleModelEnabled,
    createProvider,
    createModel,
    selectProvider,
    selectModel,
    selectReasoning,
    getProviderSpec,
    getReasoningRequest,
    enabledModels,
  };
}
