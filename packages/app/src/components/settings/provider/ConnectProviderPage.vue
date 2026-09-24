<template>
  <div class="flex items-center gap-1">
    <button
      class="h-[26px] rounded-md px-2.5 text-[12.5px] transition-colors"
      :class="
        connectMode === 'pick'
          ? 'bg-neutral-2 text-strong font-semibold'
          : 'text-dim hover:bg-neutral-1'
      "
      @click="connectMode = 'pick'"
    >
      从列表连接
    </button>
    <button
      class="h-[26px] rounded-md px-2.5 text-[12.5px] transition-colors"
      :class="
        connectMode === 'manual'
          ? 'bg-neutral-2 text-strong font-semibold'
          : 'text-dim hover:bg-neutral-1'
      "
      @click="connectMode = 'manual'"
    >
      手动添加
    </button>
  </div>

  <!-- 模式一：从已有（未连接）Provider 中选择并填 Key -->
  <template v-if="connectMode === 'pick'">
    <div
      class="mt-4 max-h-[260px] overflow-y-auto border border-line-soft rounded-lg bg-surface"
    >
      <button
        v-for="provider in unconnectedProviders"
        :key="provider.id"
        class="h-9 w-full flex items-center gap-2 border-b border-line-soft px-3 text-left text-[12.5px] transition-colors last:border-b-0"
        :class="
          pickId === provider.id
            ? 'bg-accent-3 text-neutral-10'
            : 'text-dim hover:bg-neutral-1 hover:text-neutral-10'
        "
        @click="pickId = provider.id"
      >
        <span class="truncate">{{ provider.name }}</span>
        <span
          class="ml-auto flex-none rounded bg-neutral-2 px-1.5 py-px text-[10.5px] text-dim font-medium leading-[16px]"
        >
          {{ provider.adapter_type }}
        </span>
      </button>
      <p
        v-if="unconnectedProviders.length === 0"
        class="px-3 py-4 text-center text-[12px] text-faint"
      >
        所有内置 Provider 都已连接，可用「手动添加」接入新的。
      </p>
    </div>

    <template v-if="picked">
      <div
        class="mt-5 border-b border-line border-dashed pb-1.5 text-[11px] text-dim tracking-[0.5px] font-mono"
      >
        Connect {{ picked.name }}
      </div>
      <div
        class="flex items-center gap-[18px] border-b border-line-soft border-dashed py-2.5"
      >
        <div class="min-w-0 flex-1">
          <div class="text-[13px] text-strong font-semibold">API Key</div>
          <div class="mt-0.5 text-[12px] text-dim">
            填入 {{ picked.name }} 的 API Key，保存后即完成连接。
          </div>
        </div>
        <input
          v-model="pickKey"
          :type="showPickKey ? 'text' : 'password'"
          placeholder="Enter API key..."
          class="h-[26px] w-[240px] flex-none control-surface px-2.5 text-[12px] control-focus"
          @keydown.enter="connectPicked"
        />
        <button
          class="h-[26px] flex-none control-ghost rounded-md px-2 text-[12px] control-focus"
          @click="showPickKey = !showPickKey"
        >
          {{ showPickKey ? "Hide" : "Show" }}
        </button>
        <button
          class="h-[26px] flex-none control-solid rounded-md px-3 text-[12px] font-medium control-focus transition-colors disabled:opacity-50"
          :disabled="!pickKey.trim()"
          @click="connectPicked"
        >
          Connect
        </button>
      </div>
    </template>
  </template>

  <!-- 模式二：手动添加（Anthropic / OpenAI 兼容） -->
  <template v-else>
    <div class="mt-4 flex items-center gap-1">
      <button
        v-for="t in PROVIDER_TYPES"
        :key="t.type"
        class="h-[26px] rounded-md px-2.5 text-[12.5px] transition-colors"
        :class="
          addForm.type === t.type
            ? 'bg-neutral-2 text-strong font-semibold'
            : 'text-dim hover:bg-neutral-1'
        "
        @click="switchType(t.type)"
      >
        {{ t.label }}
      </button>
    </div>

    <div class="mt-4 space-y-4">
      <label class="block">
        <span class="text-[13px] text-strong font-semibold">
          Provider Name
        </span>
        <span class="ml-1 text-[12px] text-danger-10">*</span>
        <div class="mt-0.5 text-[12px] text-dim">
          用于识别该 Provider 的唯一名称。
        </div>
        <input
          v-model="addForm.name"
          :placeholder="addForm.type === 'anthropic' ? 'Anthropic' : 'OpenAI'"
          class="mt-2 h-[30px] w-full control-surface px-3 text-[13px] control-focus"
        />
      </label>
      <label class="block">
        <span class="text-[13px] text-strong font-semibold"> API URL </span>
        <span class="ml-1 text-[12px] text-danger-10">*</span>
        <div class="mt-0.5 text-[12px] text-dim">兼容 API 的 base URL。</div>
        <input
          v-model="addForm.base_url"
          :placeholder="
            addForm.type === 'anthropic'
              ? 'https://api.anthropic.com/v1'
              : 'https://api.openai.com/v1'
          "
          class="mt-2 h-[30px] w-full control-surface px-3 text-[13px] control-focus"
        />
      </label>
      <label class="block">
        <span class="text-[13px] text-strong font-semibold"> API Key </span>
        <span class="ml-1 text-[12px] text-danger-10">*</span>
        <div class="mt-0.5 text-[12px] text-dim">
          调用该 Provider 使用的密钥。
        </div>
        <input
          v-model="addForm.api_key"
          type="password"
          placeholder="sk-..."
          class="mt-2 h-[30px] w-full control-surface px-3 text-[13px] control-focus"
        />
      </label>
      <p v-if="addError" class="text-[12px] text-danger-10">{{ addError }}</p>
      <div class="flex items-center gap-2">
        <button
          class="h-[28px] control-solid rounded-md px-4 text-[12.5px] font-medium control-focus transition-colors disabled:opacity-50"
          :disabled="!canSubmitProvider || creating"
          @click="submitProvider"
        >
          {{ creating ? "Creating…" : "Create Provider" }}
        </button>
        <button
          class="h-[28px] control-ghost rounded-md px-3 text-[12.5px] control-focus"
          @click="emit('cancel')"
        >
          Cancel
        </button>
      </div>
    </div>
  </template>
</template>

<script setup lang="ts">
import { computed, onBeforeUnmount, ref } from "vue";
import { useProviders } from "@/composables/useProviders";

/**
 * Connect Provider（从列表连接 / 手动添加）。两个模式的表单状态归本页
 * 所有（进入即重置）；异步提交在离开页面后只丢弃结果，不回写。
 */
const props = defineProps<{
  query: string;
}>();

const emit = defineEmits<{
  /** 连接/创建成功：壳层跳到该 provider 的详情页。 */
  connected: [providerId: string];
  cancel: [];
}>();

const { providers, updateApiKey, createProvider } = useProviders();

let disposed = false;
onBeforeUnmount(() => {
  disposed = true;
});

const connectMode = ref<"pick" | "manual">("pick");
const pickId = ref<string | null>(null);
const pickKey = ref("");
const showPickKey = ref(false);
const PROVIDER_TYPES = [
  { type: "anthropic", label: "Anthropic" },
  { type: "openai", label: "OpenAI 兼容" },
] as const;
const TYPE_DEFAULT_URLS = {
  anthropic: "https://api.anthropic.com/v1",
  openai: "https://api.openai.com/v1",
} as const;
const addForm = ref({
  type: "openai" as (typeof PROVIDER_TYPES)[number]["type"],
  name: "",
  base_url: "",
  api_key: "",
});
const addError = ref<string | null>(null);
const creating = ref(false);

const unconnectedProviders = computed(() => {
  const q = props.query.trim().toLowerCase();
  return providers.value
    .filter(
      (provider) =>
        !provider.api_key_set &&
        (!q || provider.name.toLowerCase().includes(q)),
    )
    .sort((a, b) => a.name.localeCompare(b.name));
});

const picked = computed(
  () =>
    unconnectedProviders.value.find(
      (provider) => provider.id === pickId.value,
    ) ?? null,
);

async function connectPicked() {
  const provider = picked.value;
  const key = pickKey.value.trim();
  if (!provider || !key) return;
  const ok = await updateApiKey(provider.id, key);
  if (disposed) return;
  if (ok) {
    emit("connected", provider.id);
  }
}

function switchType(type: (typeof PROVIDER_TYPES)[number]["type"]) {
  addForm.value.type = type;
  // 切换类型时填充对应的默认 base URL
  addForm.value.base_url = TYPE_DEFAULT_URLS[type];
}

const canSubmitProvider = computed(
  () =>
    addForm.value.name.trim().length > 0 &&
    addForm.value.base_url.trim().length > 0 &&
    addForm.value.api_key.trim().length > 0,
);

// 提到模块作用域，避免每次调用重新编译正则
const SLUG_INVALID_CHARS = /[^a-z0-9]+/g;
const SLUG_EDGE_DASHES = /(^-|-$)/g;

function slugify(name: string): string {
  return (
    name
      .trim()
      .toLowerCase()
      .replaceAll(SLUG_INVALID_CHARS, "-")
      .replaceAll(SLUG_EDGE_DASHES, "") || "custom"
  );
}

async function submitProvider() {
  creating.value = true;
  addError.value = null;
  const name = addForm.value.name.trim();
  const created = await createProvider({
    id: slugify(name),
    name,
    base_url: addForm.value.base_url.trim(),
    api_key: addForm.value.api_key.trim(),
  });
  creating.value = false;
  if (disposed) return;
  if (!created) {
    addError.value = "创建失败：ID 可能已被占用，换个名称试试。";
    return;
  }
  emit("connected", created.id);
}
</script>
