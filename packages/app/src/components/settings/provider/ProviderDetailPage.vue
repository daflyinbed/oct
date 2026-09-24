<template>
  <div v-if="selected" class="flex items-center gap-2">
    <DialogTitle class="text-[16.5px] text-strong font-semibold">
      {{ selected.name }}
    </DialogTitle>
    <span
      class="rounded bg-success-7 px-1.5 py-px text-[10.5px] text-success-contrast font-medium leading-[16px]"
    >
      已连接
    </span>
  </div>
  <p v-if="selected" class="mt-1 text-[12px] text-dim">
    <span class="font-mono">{{ selected.adapter_type }}</span>
    <template v-if="selected.base_url">
      · <span class="font-mono">{{ selected.base_url }}</span>
    </template>
  </p>

  <template v-if="selected">
    <div
      class="mt-6 border-b border-line border-dashed pb-1.5 text-[11px] text-dim tracking-[0.5px] font-mono"
    >
      Connection
    </div>
    <div
      class="flex items-center gap-[18px] border-b border-line-soft border-dashed py-2.5"
    >
      <div class="min-w-0 flex-1">
        <div class="text-[13px] text-strong font-semibold">API Key</div>
        <div class="mt-0.5 text-[12px] text-dim">
          已配置。输入新 Key 并保存即可替换。
        </div>
      </div>
      <input
        :type="showKey ? 'text' : 'password'"
        placeholder="••••••••（已配置）"
        class="h-[26px] w-[240px] flex-none control-surface px-2.5 text-[12px] control-focus"
        :value="keyInput"
        @input="keyInput = ($event.target as HTMLInputElement).value"
        @keydown.enter="saveKey"
      />
      <button
        class="h-[26px] flex-none control-ghost rounded-md px-2 text-[12px] control-focus"
        @click="showKey = !showKey"
      >
        {{ showKey ? "Hide" : "Show" }}
      </button>
      <button
        class="h-[26px] flex-none control-solid rounded-md px-3 text-[12px] font-medium control-focus transition-colors disabled:opacity-50"
        :disabled="!keyInput.trim()"
        @click="saveKey"
      >
        Save
      </button>
    </div>

    <div
      class="mt-6 border-b border-line border-dashed pb-1.5 text-[11px] text-dim tracking-[0.5px] font-mono"
    >
      Models
    </div>
    <div
      v-for="model in filteredModels"
      :key="model.model_id"
      class="flex items-center gap-[18px] border-b border-line-soft border-dashed py-2.5"
    >
      <div class="min-w-0 flex-1">
        <div class="truncate text-[13px] text-strong font-semibold">
          {{ model.name }}
        </div>
        <div class="mt-0.5 text-[11.5px] text-faint font-mono">
          {{ model.model_id }}
        </div>
      </div>
      <div class="flex flex-none items-center gap-1">
        <span
          v-if="model.limit_context"
          class="rounded bg-neutral-2 px-1 py-px text-[10px] text-dim"
        >
          {{ formatTokens(model.limit_context) }}
        </span>
        <span
          v-if="model.tool_call"
          class="rounded bg-neutral-2 px-1 py-px text-[10px] text-dim"
          >tools</span
        >
        <span
          v-if="model.reasoning"
          class="rounded bg-neutral-2 px-1 py-px text-[10px] text-dim"
          >thinking</span
        >
      </div>
      <button
        class="relative h-[19px] w-[34px] flex-none rounded-full transition-colors"
        :class="model.is_enabled ? 'bg-accent-7' : 'bg-neutral-2'"
        role="switch"
        :aria-checked="model.is_enabled"
        :title="model.is_enabled ? '在模型选择器中隐藏' : '在模型选择器中显示'"
        @click="
          toggleModelEnabled(selected.id, model.model_id, !model.is_enabled)
        "
      >
        <span
          class="absolute left-[2px] top-[2px] h-[15px] w-[15px] rounded-full bg-white shadow-[0_1px_3px_rgba(0,0,0,0.3)] transition-transform"
          :class="model.is_enabled ? 'translate-x-[15px]' : ''"
        />
      </button>
    </div>
    <p v-if="filteredModels.length === 0" class="py-4 text-[12px] text-faint">
      无匹配的模型
    </p>

    <!-- 自定义 Provider：手动登记模型 -->
    <div v-if="selected.source === 'custom'" class="mt-4">
      <div class="text-[12.5px] text-neutral-10 font-semibold">Add Model</div>
      <div
        class="mt-2 flex items-end gap-2 border border-line-soft rounded-lg bg-surface p-3"
      >
        <label class="block min-w-0 flex-1">
          <span class="text-[11px] text-dim">Model ID</span>
          <input
            v-model="modelForm.model_id"
            placeholder="e.g. gpt-5"
            class="mt-1 h-[26px] w-full control-surface px-2.5 text-[12px] control-focus"
          />
        </label>
        <label class="block min-w-0 flex-1">
          <span class="text-[11px] text-dim">Display Name</span>
          <input
            v-model="modelForm.name"
            placeholder="e.g. GPT-5"
            class="mt-1 h-[26px] w-full control-surface px-2.5 text-[12px] control-focus"
          />
        </label>
        <label class="block w-[120px] flex-none">
          <span class="text-[11px] text-dim">Context</span>
          <input
            v-model="modelForm.limit_context"
            placeholder="128000"
            class="mt-1 h-[26px] w-full control-surface px-2.5 text-[12px] control-focus"
          />
        </label>
        <button
          class="h-[26px] flex-none control-solid rounded-md px-3 text-[12px] font-medium control-focus disabled:opacity-50"
          :disabled="!modelForm.model_id.trim()"
          @click="submitModel"
        >
          Add
        </button>
      </div>
    </div>
  </template>
</template>

<script setup lang="ts">
import { DialogTitle } from "reka-ui";
import { computed, ref, watch } from "vue";
import { useProviders } from "@/composables/useProviders";

/** Provider 详情（Key 替换 + 模型可见性 + 自定义 provider 登记模型）。 */
const props = defineProps<{
  providerId: string;
  query: string;
}>();

const emit = defineEmits<{
  /** 详情对应的 Provider 不存在时退回列表。 */
  gone: [];
}>();

const { providers, updateApiKey, toggleModelEnabled, createModel } =
  useProviders();

const selected = computed(
  () =>
    providers.value.find((provider) => provider.id === props.providerId) ??
    null,
);

// 详情页对应的 Provider 不存在时退回列表
watch(selected, (value) => {
  if (!value) emit("gone");
});

/* Key 输入归本页所有（进入清空），不再跨 provider 共享残留。 */
const keyInput = ref("");
const showKey = ref(false);

watch(
  () => props.providerId,
  () => {
    keyInput.value = "";
    showKey.value = false;
  },
);

const modelForm = ref({ model_id: "", name: "", limit_context: "" });

const filteredModels = computed(() => {
  const provider = selected.value;
  if (!provider) return [];
  const q = props.query.trim().toLowerCase();
  if (!q) return provider.models;
  return provider.models.filter(
    (model) =>
      model.name.toLowerCase().includes(q) ||
      model.model_id.toLowerCase().includes(q),
  );
});

function saveKey() {
  const key = keyInput.value.trim();
  if (!key || !selected.value) return;
  void updateApiKey(selected.value.id, key);
  keyInput.value = "";
}

async function submitModel() {
  if (!selected.value) return;
  const modelId = modelForm.value.model_id.trim();
  if (!modelId) return;
  const context = Number.parseInt(modelForm.value.limit_context, 10);
  const ok = await createModel(selected.value.id, {
    model_id: modelId,
    name: modelForm.value.name.trim() || modelId,
    limit_context: Number.isFinite(context) ? context : null,
  });
  if (ok) {
    modelForm.value = { model_id: "", name: "", limit_context: "" };
  }
}

function formatTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(0)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(0)}K`;
  return String(n);
}
</script>
