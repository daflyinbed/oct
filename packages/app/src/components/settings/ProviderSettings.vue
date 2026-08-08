<template>
  <div
    v-if="visible"
    class="fixed inset-0 z-50 flex items-start justify-center pt-[10vh]"
  >
    <div class="absolute inset-0 bg-overlay" @click="$emit('close')" />
    <div
      class="relative w-full max-w-2xl max-h-[75vh] flex flex-col border border-neutral-5 rounded-lg bg-panel shadow-xl"
    >
      <div
        class="flex items-center justify-between px-6 py-4 border-b border-neutral-5"
      >
        <h2 class="text-[1.1rem] font-semibold text-neutral-10">
          Provider Settings
        </h2>
        <button
          class="control-ghost control-focus px-2 py-1 text-[1.1rem]"
          @click="$emit('close')"
        >
          ✕
        </button>
      </div>

      <div class="flex-1 overflow-y-auto p-6 space-y-4">
        <div
          v-if="providers.length === 0 && !loading"
          class="text-center py-8 text-neutral-10/60"
        >
          No providers found. Run <code>seed</code> to load built-in providers.
        </div>

        <div
          v-for="provider in providers"
          :key="provider.id"
          class="border border-neutral-5 rounded-md p-4"
        >
          <div class="flex items-center justify-between mb-3">
            <div class="flex items-center gap-2">
              <span class="text-[1rem] font-semibold text-neutral-10">{{
                provider.name
              }}</span>
              <span
                class="rounded-sm px-1.5 py-0.5 text-[0.875rem] font-medium"
                :class="
                  provider.api_key_set
                    ? 'bg-success-7 text-success-contrast'
                    : 'bg-warning-7 text-warning-contrast'
                "
              >
                {{ provider.api_key_set ? "Key set" : "No key" }}
              </span>
              <span
                class="rounded-sm bg-surface px-1.5 py-0.5 text-[0.875rem] font-medium text-neutral-10/60"
              >
                {{ provider.adapter_type }}
              </span>
            </div>
          </div>

          <div class="flex items-center gap-2 mb-3">
            <input
              :type="showKeys[provider.id] ? 'text' : 'password'"
              :placeholder="
                provider.api_key_set
                  ? 'Key configured — enter new to replace'
                  : 'Enter API key...'
              "
              class="control-surface control-focus flex-1 bg-background px-3 py-1.5 text-[0.875rem]"
              :value="apiKeyInputs[provider.id] ?? ''"
              @input="
                (e: Event) => {
                  apiKeyInputs[provider.id] = (
                    e.target as HTMLInputElement
                  ).value;
                }
              "
            />
            <button
              class="control-ghost control-focus border border-neutral-5 px-2 py-1.5 text-[0.875rem] font-medium"
              @click="showKeys[provider.id] = !showKeys[provider.id]"
            >
              {{ showKeys[provider.id] ? "Hide" : "Show" }}
            </button>
            <button
              class="control-solid control-focus px-3 py-1.5 text-[0.875rem] font-medium transition-colors disabled:opacity-50"
              :disabled="!apiKeyInputs[provider.id]?.trim()"
              @click="handleSaveKey(provider.id)"
            >
              Save
            </button>
          </div>

          <div v-if="provider.models.length > 0" class="space-y-1">
            <div
              v-for="model in provider.models"
              :key="model.model_id"
              class="flex items-center justify-between rounded-sm px-3 py-1.5 text-[0.875rem]"
            >
              <div class="flex items-center gap-2">
                <input
                  type="checkbox"
                  :checked="model.is_enabled"
                  class="accent-accent-7"
                  @change="
                    handleToggleModel(
                      provider.id,
                      model.model_id,
                      ($event.target as HTMLInputElement).checked,
                    )
                  "
                />
                <span class="text-neutral-10">{{ model.name }}</span>
                <span class="text-[0.875rem] text-neutral-10/60">{{
                  model.model_id
                }}</span>
              </div>
              <div
                class="flex items-center gap-2 text-[0.875rem] text-neutral-10/60"
              >
                <span v-if="model.limit_context"
                  >{{ formatTokens(model.limit_context) }} ctx</span
                >
                <span v-if="model.tool_call">🛠</span>
                <span v-if="model.reasoning">💭</span>
              </div>
            </div>
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { ref } from "vue";
import type { components } from "@/api/schema";

type ProviderResponse = components["schemas"]["ProviderResponse"];

const _props = defineProps<{
  visible: boolean;
  providers: readonly ProviderResponse[];
  loading: boolean;
}>();
const emit = defineEmits<{
  close: [];
  saveKey: [providerId: string, apiKey: string];
  toggleModel: [providerId: string, modelId: string, enabled: boolean];
}>();

const apiKeyInputs = ref({} as Record<string, string>);
const showKeys = ref({} as Record<string, boolean>);

function handleSaveKey(providerId: string) {
  const key = apiKeyInputs.value[providerId]?.trim();
  if (!key) return;
  emit("saveKey", providerId, key);
  delete apiKeyInputs.value[providerId];
}

function handleToggleModel(
  providerId: string,
  modelId: string,
  enabled: boolean,
) {
  emit("toggleModel", providerId, modelId, enabled);
}

function formatTokens(n: number | null | undefined): string {
  if (!n) return "";
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(0)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(0)}K`;
  return String(n);
}
</script>
