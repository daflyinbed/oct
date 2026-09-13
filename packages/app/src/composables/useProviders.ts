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
    loading.value = false;
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
    toggleModelEnabled,
    selectProvider,
    selectModel,
    getProviderSpec,
    enabledModels,
  };
}
