import { readonly, ref, shallowReadonly } from "vue";
import client from "@/api/client";
import type { components } from "@/api/schema";

type ProviderResponse = components["schemas"]["ProviderResponse"];
type ModelSummary = components["schemas"]["ModelSummary"];

const providers = ref<ProviderResponse[]>([]);
const loading = ref(false);
const selectedProviderId = ref<string | null>(null);
const selectedModelId = ref<string | null>(null);

export function useProviders() {
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
    getProviderSpec,
    enabledModels,
  };
}
