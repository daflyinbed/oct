<template>
  <div
    v-for="provider in connectedProviders"
    :key="provider.id"
    class="border-b border-line-soft border-dashed py-3"
  >
    <div class="flex items-center gap-2">
      <span class="h-1.5 w-1.5 flex-none rounded-full bg-success-10" />
      <span class="text-[13px] text-strong font-semibold">{{
        provider.name
      }}</span>
      <span
        class="rounded bg-neutral-2 px-1.5 py-px text-[10.5px] text-dim font-medium leading-[16px]"
      >
        {{ provider.adapter_type }}
      </span>
      <span class="flex-1" />
      <button
        class="h-[26px] inline-flex flex-none items-center gap-1.5 control-surface rounded-md px-2.5 text-[12px] font-medium control-focus"
        @click="emit('open', provider.id)"
      >
        Configure
        <i-lucide-chevron-right class="h-3 w-3 text-dim" />
      </button>
    </div>
    <div class="mt-1.5 pl-3.5 text-[12px] text-dim">
      已启用 {{ enabledCount(provider) }} / {{ provider.models.length }} 个模型
    </div>
  </div>

  <div
    v-if="connectedProviders.length === 0 && !loading"
    class="py-10 text-center"
  >
    <p class="text-[12.5px] text-faint">尚未连接任何 Provider。</p>
    <button
      class="mt-3 h-[26px] inline-flex items-center gap-1.5 control-surface rounded-md px-2.5 text-[12px] font-medium control-focus"
      @click="emit('connect')"
    >
      <i-lucide-plus class="h-3 w-3" />
      Connect Provider
    </button>
  </div>
  <p v-if="loading" class="py-8 text-center text-[12.5px] text-faint">
    Loading…
  </p>
</template>

<script setup lang="ts">
import { computed } from "vue";
import { useProviders } from "@/composables/useProviders";
import type { components } from "@/api/schema";

type ProviderResponse = components["schemas"]["ProviderResponse"];

const props = defineProps<{
  query: string;
}>();

const emit = defineEmits<{
  connect: [];
  open: [providerId: string];
}>();

const { providers, loading } = useProviders();

const connectedProviders = computed(() => {
  const q = props.query.trim().toLowerCase();
  return providers.value
    .filter(
      (provider) =>
        provider.api_key_set && (!q || provider.name.toLowerCase().includes(q)),
    )
    .sort((a, b) => a.name.localeCompare(b.name));
});

function enabledCount(provider: ProviderResponse): number {
  return provider.models.filter((model) => model.is_enabled).length;
}
</script>
