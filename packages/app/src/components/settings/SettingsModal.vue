<template>
  <DialogRoot :open="visible" @update:open="handleUpdateOpen">
    <DialogPortal>
      <DialogOverlay class="fixed inset-0 z-50 bg-overlay" />
      <DialogContent
        class="fixed left-1/2 top-1/2 z-50 h-[650px] max-h-[calc(100vh-72px)] max-w-[calc(100vw-72px)] w-[940px] flex overflow-hidden border border-neutral-5 rounded-xl bg-panel shadow-[var(--oct-shadow)] -translate-x-1/2 -translate-y-1/2"
      >
        <!-- 左侧：搜索 + 分类导航（provider 设置是 AI 分类下的子项） -->
        <div
          class="w-[216px] flex flex-none flex-col border-r border-line-soft bg-titlebar"
        >
          <div class="flex-none p-3 pb-2">
            <div
              class="h-7 flex items-center gap-[7px] border border-line-soft rounded-md bg-surface px-[9px] text-faint transition-colors focus-within:border-accent-6"
            >
              <i-lucide-search class="h-3 w-3 flex-none" />
              <input
                v-model="query"
                placeholder="Search settings…"
                class="min-w-0 flex-1 border-none bg-transparent text-[12.5px] text-neutral-10 outline-none placeholder:text-faint"
              />
            </div>
          </div>
          <div class="flex-1 overflow-y-auto px-1.5 pb-3">
            <button
              class="h-7 w-full flex items-center gap-2 rounded-md px-2 text-left text-[12.5px] text-neutral-10 transition-colors"
              :class="view.name !== 'ai' ? 'bg-neutral-1 font-semibold' : ''"
              @click="view = { name: 'ai' }"
            >
              <i-lucide-chevron-right
                class="h-2.5 w-2.5 flex-none text-faint"
              />
              AI
            </button>
          </div>
        </div>

        <!-- 右侧：顶栏（面包屑）+ 内容 -->
        <div class="min-w-0 flex flex-1 flex-col">
          <div
            class="h-[46px] flex flex-none items-center gap-2 border-b border-line-soft px-3 pl-4"
          >
            <template v-if="view.name !== 'ai'">
              <button
                class="h-[22px] w-[22px] flex-none control-ghost rounded-md control-focus"
                title="返回"
                @click="goBack"
              >
                <i-lucide-arrow-left class="h-3.5 w-3.5" />
              </button>
              <button
                class="text-[12.5px] text-dim transition-colors hover:text-neutral-10"
                @click="view = { name: 'ai' }"
              >
                AI
              </button>
              <span class="text-[12.5px] text-faint">/</span>
              <button
                class="text-[12.5px] text-dim transition-colors hover:text-neutral-10"
                @click="view = { name: 'providers' }"
              >
                LLM Providers
              </button>
              <template v-if="view.name === 'provider'">
                <span class="text-[12.5px] text-faint">/</span>
                <span class="text-[12.5px] text-neutral-10 font-semibold">{{
                  selected?.name
                }}</span>
              </template>
              <template v-else-if="view.name === 'connect-provider'">
                <span class="text-[12.5px] text-faint">/</span>
                <span class="text-[12.5px] text-neutral-10 font-semibold"
                  >Connect Provider</span
                >
              </template>
            </template>
            <span v-else class="text-[12.5px] text-neutral-10 font-semibold">
              AI
            </span>
            <span class="flex-1" />
            <button
              v-if="view.name === 'providers'"
              class="h-[26px] inline-flex items-center gap-1.5 control-surface rounded-md px-2.5 text-[12px] font-medium control-focus"
              @click="startConnect"
            >
              <i-lucide-plus class="h-3 w-3" />
              Connect Provider
            </button>
            <DialogClose
              class="h-[26px] w-[26px] control-ghost rounded-md text-[12px] control-focus"
              title="关闭（Esc）"
            >
              ✕
            </DialogClose>
          </div>

          <div class="flex-1 overflow-y-auto px-7 pb-12 pt-[18px]">
            <!-- ============ AI 根页：LLM Providers 子项 ============ -->
            <template v-if="view.name === 'ai'">
              <DialogTitle class="text-[16.5px] text-strong font-semibold">
                AI
              </DialogTitle>
              <p class="mt-1 text-[12px] text-dim">
                模型 Provider 与聊天标签页可用模型的配置入口。
              </p>
              <div
                class="mt-6 border-b border-line border-dashed pb-1.5 text-[11px] text-dim tracking-[0.5px] font-mono"
              >
                General
              </div>
              <div
                class="flex items-center gap-[18px] border-b border-line-soft border-dashed py-3"
              >
                <div class="min-w-0 flex-1">
                  <div class="text-[13px] text-strong font-semibold">
                    LLM Providers
                  </div>
                  <div class="mt-0.5 text-[12px] text-dim">
                    连接模型 Provider，并选择聊天标签页中显示哪些模型。
                  </div>
                </div>
                <button
                  class="h-[26px] inline-flex flex-none items-center gap-1.5 control-surface rounded-md px-2.5 text-[12px] font-medium control-focus"
                  @click="view = { name: 'providers' }"
                >
                  Configure
                  <i-lucide-chevron-right class="h-3 w-3 text-dim" />
                </button>
              </div>
            </template>

            <!-- ============ 已连接的 Provider 列表 ============ -->
            <template v-else-if="view.name === 'providers'">
              <div
                v-for="provider in connectedProviders"
                :key="provider.id"
                class="border-b border-line-soft border-dashed py-3"
              >
                <div class="flex items-center gap-2">
                  <span
                    class="h-1.5 w-1.5 flex-none rounded-full bg-success-10"
                  />
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
                    @click="view = { name: 'provider', id: provider.id }"
                  >
                    Configure
                    <i-lucide-chevron-right class="h-3 w-3 text-dim" />
                  </button>
                </div>
                <div class="mt-1.5 pl-3.5 text-[12px] text-dim">
                  已启用 {{ enabledCount(provider) }} /
                  {{ provider.models.length }} 个模型
                </div>
              </div>

              <div
                v-if="connectedProviders.length === 0 && !loading"
                class="py-10 text-center"
              >
                <p class="text-[12.5px] text-faint">尚未连接任何 Provider。</p>
                <button
                  class="mt-3 h-[26px] inline-flex items-center gap-1.5 control-surface rounded-md px-2.5 text-[12px] font-medium control-focus"
                  @click="startConnect"
                >
                  <i-lucide-plus class="h-3 w-3" />
                  Connect Provider
                </button>
              </div>
              <p
                v-if="loading"
                class="py-8 text-center text-[12.5px] text-faint"
              >
                Loading…
              </p>
            </template>

            <!-- ============ Connect Provider（选已有 / 手动添加） ============ -->
            <template v-else-if="view.name === 'connect-provider'">
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
                      <div class="text-[13px] text-strong font-semibold">
                        API Key
                      </div>
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
                      :placeholder="
                        addForm.type === 'anthropic' ? 'Anthropic' : 'OpenAI'
                      "
                      class="mt-2 h-[30px] w-full control-surface px-3 text-[13px] control-focus"
                    />
                  </label>
                  <label class="block">
                    <span class="text-[13px] text-strong font-semibold">
                      API URL
                    </span>
                    <span class="ml-1 text-[12px] text-danger-10">*</span>
                    <div class="mt-0.5 text-[12px] text-dim">
                      兼容 API 的 base URL。
                    </div>
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
                    <span class="text-[13px] text-strong font-semibold">
                      API Key
                    </span>
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
                  <p v-if="addError" class="text-[12px] text-danger-10">
                    {{ addError }}
                  </p>
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
                      @click="view = { name: 'providers' }"
                    >
                      Cancel
                    </button>
                  </div>
                </div>
              </template>
            </template>

            <!-- ============ Provider 详情（Key + 模型可见性） ============ -->
            <template v-else-if="view.name === 'provider' && selected">
              <div class="flex items-center gap-2">
                <DialogTitle class="text-[16.5px] text-strong font-semibold">
                  {{ selected.name }}
                </DialogTitle>
                <span
                  class="rounded bg-success-7 px-1.5 py-px text-[10.5px] text-success-contrast font-medium leading-[16px]"
                >
                  已连接
                </span>
              </div>
              <p class="mt-1 text-[12px] text-dim">
                <span class="font-mono">{{ selected.adapter_type }}</span>
                <template v-if="selected.base_url">
                  · <span class="font-mono">{{ selected.base_url }}</span>
                </template>
              </p>

              <div
                class="mt-6 border-b border-line border-dashed pb-1.5 text-[11px] text-dim tracking-[0.5px] font-mono"
              >
                Connection
              </div>
              <div
                class="flex items-center gap-[18px] border-b border-line-soft border-dashed py-2.5"
              >
                <div class="min-w-0 flex-1">
                  <div class="text-[13px] text-strong font-semibold">
                    API Key
                  </div>
                  <div class="mt-0.5 text-[12px] text-dim">
                    已配置。输入新 Key 并保存即可替换。
                  </div>
                </div>
                <input
                  :type="showKeys[selected.id] ? 'text' : 'password'"
                  placeholder="••••••••（已配置）"
                  class="h-[26px] w-[240px] flex-none control-surface px-2.5 text-[12px] control-focus"
                  :value="keyInputs[selected.id] ?? ''"
                  @input="
                    (e: Event) => {
                      keyInputs[selected!.id] = (
                        e.target as HTMLInputElement
                      ).value;
                    }
                  "
                  @keydown.enter="saveKey(selected.id)"
                />
                <button
                  class="h-[26px] flex-none control-ghost rounded-md px-2 text-[12px] control-focus"
                  @click="showKeys[selected.id] = !showKeys[selected.id]"
                >
                  {{ showKeys[selected.id] ? "Hide" : "Show" }}
                </button>
                <button
                  class="h-[26px] flex-none control-solid rounded-md px-3 text-[12px] font-medium control-focus transition-colors disabled:opacity-50"
                  :disabled="!keyInputs[selected.id]?.trim()"
                  @click="saveKey(selected.id)"
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
                  :title="
                    model.is_enabled
                      ? '在模型选择器中隐藏'
                      : '在模型选择器中显示'
                  "
                  @click="
                    toggleModelEnabled(
                      selected.id,
                      model.model_id,
                      !model.is_enabled,
                    )
                  "
                >
                  <span
                    class="absolute left-[2px] top-[2px] h-[15px] w-[15px] rounded-full bg-white shadow-[0_1px_3px_rgba(0,0,0,0.3)] transition-transform"
                    :class="model.is_enabled ? 'translate-x-[15px]' : ''"
                  />
                </button>
              </div>
              <p
                v-if="filteredModels.length === 0"
                class="py-4 text-[12px] text-faint"
              >
                无匹配的模型
              </p>

              <!-- 自定义 Provider：手动登记模型 -->
              <div v-if="selected.source === 'custom'" class="mt-4">
                <div class="text-[12.5px] text-neutral-10 font-semibold">
                  Add Model
                </div>
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
                    @click="submitModel(selected.id)"
                  >
                    Add
                  </button>
                </div>
              </div>
            </template>
          </div>
        </div>
        <DialogDescription class="sr-only">
          连接 Provider 并选择聊天标签页中显示的模型。
        </DialogDescription>
      </DialogContent>
    </DialogPortal>
  </DialogRoot>
</template>

<script setup lang="ts">
import {
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogOverlay,
  DialogPortal,
  DialogRoot,
  DialogTitle,
} from "reka-ui";
import { computed, ref, watch } from "vue";
import { useProviders } from "@/composables/useProviders";
import type { components } from "@/api/schema";

type ProviderResponse = components["schemas"]["ProviderResponse"];

defineProps<{
  visible: boolean;
}>();

const emit = defineEmits<{
  close: [];
}>();

const {
  providers,
  loading,
  updateApiKey,
  toggleModelEnabled,
  createProvider,
  createModel,
} = useProviders();

type View =
  | { name: "ai" }
  | { name: "providers" }
  | { name: "provider"; id: string }
  | { name: "connect-provider" };

const view = ref<View>({ name: "ai" });
const query = ref("");
const keyInputs = ref({} as Record<string, string>);
const showKeys = ref({} as Record<string, boolean>);

/* ---------- Connect Provider：从列表连接 / 手动添加 ---------- */
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
const modelForm = ref({ model_id: "", name: "", limit_context: "" });

const selected = computed(
  () =>
    providers.value.find(
      (provider) =>
        provider.id === (view.value.name === "provider" ? view.value.id : null),
    ) ?? null,
);

// 详情页对应的 Provider 不存在时退回列表
watch(selected, (value) => {
  if (view.value.name === "provider" && !value) {
    view.value = { name: "providers" };
  }
});

const connectedProviders = computed(() => {
  const q = query.value.trim().toLowerCase();
  return providers.value
    .filter(
      (provider) =>
        provider.api_key_set && (!q || provider.name.toLowerCase().includes(q)),
    )
    .sort((a, b) => a.name.localeCompare(b.name));
});

const unconnectedProviders = computed(() => {
  const q = query.value.trim().toLowerCase();
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

const filteredModels = computed(() => {
  const provider = selected.value;
  if (!provider) return [];
  const q = query.value.trim().toLowerCase();
  if (!q) return provider.models;
  return provider.models.filter(
    (model) =>
      model.name.toLowerCase().includes(q) ||
      model.model_id.toLowerCase().includes(q),
  );
});

function goBack() {
  view.value =
    view.value.name === "ai" ? { name: "ai" } : { name: "providers" };
}

function startConnect() {
  connectMode.value = "pick";
  pickId.value = null;
  pickKey.value = "";
  addForm.value = { type: "openai", name: "", base_url: "", api_key: "" };
  addError.value = null;
  view.value = { name: "connect-provider" };
}

async function connectPicked() {
  const provider = picked.value;
  const key = pickKey.value.trim();
  if (!provider || !key) return;
  const ok = await updateApiKey(provider.id, key);
  if (ok) {
    pickId.value = null;
    pickKey.value = "";
    view.value = { name: "provider", id: provider.id };
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
  if (!created) {
    addError.value = "创建失败：ID 可能已被占用，换个名称试试。";
    return;
  }
  view.value = { name: "provider", id: created.id };
}

async function submitModel(providerId: string) {
  const modelId = modelForm.value.model_id.trim();
  if (!modelId) return;
  const context = Number.parseInt(modelForm.value.limit_context, 10);
  const ok = await createModel(providerId, {
    model_id: modelId,
    name: modelForm.value.name.trim() || modelId,
    limit_context: Number.isFinite(context) ? context : null,
  });
  if (ok) {
    modelForm.value = { model_id: "", name: "", limit_context: "" };
  }
}

function enabledCount(provider: ProviderResponse): number {
  return provider.models.filter((model) => model.is_enabled).length;
}

function saveKey(providerId: string) {
  const key = keyInputs.value[providerId]?.trim();
  if (!key) return;
  void updateApiKey(providerId, key);
  delete keyInputs.value[providerId];
}

function handleUpdateOpen(open: boolean) {
  if (!open) emit("close");
}

function formatTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(0)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(0)}K`;
  return String(n);
}
</script>
