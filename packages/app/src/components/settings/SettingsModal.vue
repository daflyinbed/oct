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
            <!-- 配置作用域：全局或某个项目；provider 系配置只在全局存在 -->
            <AppSelect
              :model-value="scopeId"
              :options="projectScopeOptions"
              placeholder="选择作用域"
              title="配置作用域"
              trigger-class="w-full text-neutral-10"
              @update:model-value="selectScope"
            />
            <div
              class="mt-2 h-7 flex items-center gap-[7px] border border-line-soft rounded-md bg-surface px-[9px] text-faint transition-colors focus-within:border-accent-6"
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
              <template v-if="inProviderTree">
                <button
                  class="text-[12.5px] text-dim transition-colors hover:text-neutral-10"
                  @click="view = { name: 'providers' }"
                >
                  LLM Providers
                </button>
                <template v-if="view.name === 'provider'">
                  <span class="text-[12.5px] text-faint">/</span>
                  <span class="text-[12.5px] text-neutral-10 font-semibold">{{
                    selectedProviderName
                  }}</span>
                </template>
                <template v-else-if="view.name === 'connect-provider'">
                  <span class="text-[12.5px] text-faint">/</span>
                  <span class="text-[12.5px] text-neutral-10 font-semibold"
                    >Connect Provider</span
                  >
                </template>
              </template>
              <template v-else>
                <button
                  class="text-[12.5px] text-dim transition-colors hover:text-neutral-10"
                  @click="view = { name: 'mcp' }"
                >
                  MCP Servers
                </button>
                <template v-if="view.name === 'mcp-edit'">
                  <span class="text-[12.5px] text-faint">/</span>
                  <span class="text-[12.5px] text-neutral-10 font-semibold">{{
                    editingServerName ?? "Add Server"
                  }}</span>
                </template>
              </template>
            </template>
            <span v-else class="text-[12.5px] text-neutral-10 font-semibold">
              AI
            </span>
            <span class="flex-1" />
            <button
              v-if="view.name === 'providers'"
              class="h-[26px] inline-flex items-center gap-1.5 control-surface rounded-md px-2.5 text-[12px] font-medium control-focus"
              @click="view = { name: 'connect-provider' }"
            >
              <i-lucide-plus class="h-3 w-3" />
              Connect Provider
            </button>
            <button
              v-if="view.name === 'mcp'"
              class="h-[26px] inline-flex items-center gap-1.5 control-surface rounded-md px-2.5 text-[12px] font-medium control-focus"
              @click="view = { name: 'mcp-edit', id: null }"
            >
              <i-lucide-plus class="h-3 w-3" />
              Add Server
            </button>
            <DialogClose
              class="h-[26px] w-[26px] control-ghost rounded-md text-[12px] control-focus"
              title="关闭（Esc）"
            >
              ✕
            </DialogClose>
          </div>

          <div class="flex-1 overflow-y-auto px-7 pb-12 pt-[18px]">
            <!-- ============ AI 根页：按作用域列出配置入口 ============ -->
            <template v-if="view.name === 'ai'">
              <DialogTitle class="text-[16.5px] text-strong font-semibold">
                AI
              </DialogTitle>
              <p class="mt-1 text-[12px] text-dim">
                {{
                  inProjectScope
                    ? `项目「${activeScopeProjectName}」专属的配置入口；全局配置切到「全局」作用域查看。`
                    : "模型 Provider 与聊天标签页可用模型的配置入口。"
                }}
              </p>
              <div
                class="mt-6 border-b border-line border-dashed pb-1.5 text-[11px] text-dim tracking-[0.5px] font-mono"
              >
                General
              </div>
              <div
                v-if="!inProjectScope"
                class="flex items-center gap-[18px] border-b border-line-soft border-dashed py-3"
              >
                <div class="min-w-0 flex-1">
                  <div class="flex items-center gap-2">
                    <span class="text-[13px] text-strong font-semibold">
                      LLM Providers
                    </span>
                    <span
                      class="rounded bg-neutral-2 px-1.5 py-px text-[10.5px] text-dim font-medium leading-[16px]"
                    >
                      全局
                    </span>
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
              <div
                class="flex items-center gap-[18px] border-b border-line-soft border-dashed py-3"
              >
                <div class="min-w-0 flex-1">
                  <div class="text-[13px] text-strong font-semibold">
                    MCP Servers
                  </div>
                  <div class="mt-0.5 text-[12px] text-dim">
                    {{
                      inProjectScope
                        ? "仅该项目对话可用的 MCP servers；全局配置对所有项目生效。"
                        : "全局作用域的 MCP servers，对所有项目生效；项目专属的切到某个项目作用域添加。"
                    }}
                  </div>
                </div>
                <button
                  class="h-[26px] inline-flex flex-none items-center gap-1.5 control-surface rounded-md px-2.5 text-[12px] font-medium control-focus"
                  title="配置 MCP Servers"
                  @click="view = { name: 'mcp' }"
                >
                  Configure
                  <i-lucide-chevron-right class="h-3 w-3 text-dim" />
                </button>
              </div>
            </template>

            <!-- ============ 子页：表单状态归各子页所有 ============ -->
            <ProviderListPage
              v-else-if="view.name === 'providers'"
              :query="query"
              @connect="view = { name: 'connect-provider' }"
              @open="(id) => (view = { name: 'provider', id })"
            />
            <ConnectProviderPage
              v-else-if="view.name === 'connect-provider'"
              :query="query"
              @connected="(id) => (view = { name: 'provider', id })"
              @cancel="view = { name: 'providers' }"
            />
            <ProviderDetailPage
              v-else-if="view.name === 'provider'"
              :provider-id="view.id"
              :query="query"
              @gone="view = { name: 'providers' }"
            />
            <McpListPage
              v-else-if="view.name === 'mcp'"
              :query="query"
              :scope-id="scopeId"
              :in-project-scope="inProjectScope"
              :active-scope-project-name="activeScopeProjectName"
              @add="view = { name: 'mcp-edit', id: null }"
              @edit="(server) => (view = { name: 'mcp-edit', id: server.id })"
            />
            <McpEditPage
              v-else-if="view.name === 'mcp-edit'"
              :key="editFormKey"
              :server-id="view.id"
              :project-scope-options="projectScopeOptions"
              :global-scope-value="GLOBAL_SCOPE"
              :default-project-id="inProjectScope ? scopeId : null"
              @saved="view = { name: 'mcp' }"
              @cancel="view = { name: 'mcp' }"
              @target-gone="view = { name: 'mcp' }"
            />
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
import McpEditPage from "@/components/settings/mcp/McpEditPage.vue";
import McpListPage from "@/components/settings/mcp/McpListPage.vue";
import ConnectProviderPage from "@/components/settings/provider/ConnectProviderPage.vue";
import ProviderDetailPage from "@/components/settings/provider/ProviderDetailPage.vue";
import ProviderListPage from "@/components/settings/provider/ProviderListPage.vue";
import AppSelect from "@/components/ui/AppSelect.vue";
import { useMcpServers } from "@/composables/useMcpServers";
import { useProjects } from "@/composables/useProjects";
import { useProviders } from "@/composables/useProviders";

/**
 * 设置弹窗的壳层：Dialog、作用域、搜索、导航（面包屑/返回）与深链定位。
 * 各功能页（provider 列表/连接/详情、MCP 列表/编辑）是独立子组件，表单
 * 状态归子页所有——进入即重建、离开即销毁，壳层不再手动清理一批散落
 * 的表单字段。
 */
const props = defineProps<{
  visible: boolean;
  /** 工作区当前活动项目；作为项目作用域的默认选中项。 */
  activeProjectId: string | null;
  /**
   * 非空 = 打开时直接定位到该 MCP server 的编辑表单（状态栏菜单的
   * 「配置」深链）。宿主在弹窗关闭时应清空，避免普通打开误触发定位。
   */
  focusServerId?: string | null;
}>();

const emit = defineEmits<{
  close: [];
}>();

const { providers } = useProviders();
const { servers: mcpServers, fetchServers: fetchMcpServers } = useMcpServers();
const { projects } = useProjects();

type View =
  | { name: "ai" }
  | { name: "providers" }
  | { name: "provider"; id: string }
  | { name: "connect-provider" }
  | { name: "mcp" }
  | { name: "mcp-edit"; id: string | null };

const view = ref<View>({ name: "ai" });
const query = ref("");

/* ---------- 配置作用域：单个 select，值为哨兵（全局）或项目 id。 ---------- */
/* ---------- provider 系配置只在全局有意义，项目作用域仅暴露 MCP； ---------- */
/* ---------- MCP 按 project_id 归属分区展示。 ---------- */

/** AppSelect 的 value 是 string；用哨兵值表达"全局"（project_id 为 null）。 */
const GLOBAL_SCOPE = "__global__";

/** 当前配置作用域：GLOBAL_SCOPE 哨兵（全局）或项目 id。 */
const scopeId = ref<string>(GLOBAL_SCOPE);

const inProjectScope = computed(() => scopeId.value !== GLOBAL_SCOPE);

// 所选项目失效（列表加载/变更、项目被他处删除）时回退：活动项目 > 第一个 > 全局
watch(
  [projects, () => props.activeProjectId],
  () => {
    if (scopeId.value === GLOBAL_SCOPE) return;
    const list = projects.value;
    if (list.some((p) => p.id === scopeId.value)) return;
    const byActive =
      props.activeProjectId && list.some((p) => p.id === props.activeProjectId)
        ? props.activeProjectId
        : null;
    scopeId.value = byActive ?? list[0]?.id ?? GLOBAL_SCOPE;
  },
  { immediate: true },
);

const activeScopeProjectName = computed(
  () => projects.value.find((p) => p.id === scopeId.value)?.name ?? "",
);

const projectScopeOptions = computed(() => [
  { value: GLOBAL_SCOPE, label: "全局（所有项目）" },
  ...projects.value.map((p) => ({ value: p.id, label: p.name })),
]);

function selectScope(value: string) {
  if (value === scopeId.value) return;
  scopeId.value = value;
  const name = view.value.name;
  if (
    name === "providers" ||
    name === "provider" ||
    name === "connect-provider"
  ) {
    // provider 系视图只在全局作用域存在
    view.value = { name: "ai" };
  } else if (name === "mcp-edit") {
    // 半截表单不跨作用域携带，回到目标作用域的列表
    view.value = { name: "mcp" };
  }
  // ai / mcp 视图跨作用域有效，原地保留（列表按新作用域响应式重过滤）
}

/* ---------- 面包屑 ---------- */

const inProviderTree = computed(() =>
  ["providers", "provider", "connect-provider"].includes(view.value.name),
);

// 判别收窄先落到 const 上（闭包内的 view.value 访问不保留收窄）。
const selectedProviderName = computed(() => {
  const current = view.value;
  if (current.name !== "provider") return "";
  return (
    providers.value.find((provider) => provider.id === current.id)?.name ?? ""
  );
});

const editingServerName = computed(() => {
  const current = view.value;
  if (current.name !== "mcp-edit" || !current.id) return null;
  return mcpServers.value.find((s) => s.id === current.id)?.name ?? null;
});

function goBack() {
  if (view.value.name === "ai") {
    view.value = { name: "ai" };
  } else if (view.value.name === "mcp" || view.value.name === "mcp-edit") {
    view.value = { name: "mcp" };
  } else {
    view.value = { name: "providers" };
  }
}

/* ---------- MCP 列表数据与进入/重开时的拉取 ---------- */

// 进入 mcp 视图时拉取列表（连接状态是实时派生值，每次进入都刷新）。
// 离开 mcp-edit（Cancel/goBack/切作用域/编辑目标失效退回）时子页整体
// 卸载，其表单/OAuth 状态随之销毁，无需壳层手动清理。
watch(
  () => view.value.name,
  (name) => {
    if (name === "mcp") void fetchMcpServers();
  },
);

// 弹窗重开时若停在 mcp 系视图同样要拉新：view.name 未变不会触发上面的
// watch，会渲染模块级缓存的陈旧状态。两条 watch 触发条件互斥（开合只变
// visible、切视图只变 view.name），不会重复请求。mcp-edit 一并拉新是为了
// 让「编辑目标已被他处删除时退回列表」的兜底（子页 watch）拿到最新列表。
watch(
  () => props.visible,
  (open) => {
    if (open && (view.value.name === "mcp" || view.value.name === "mcp-edit")) {
      void fetchMcpServers();
    }
  },
);

/* ---------- 状态栏菜单「配置」深链：打开即定位到指定 server 的编辑表单 ---------- */
watch(
  () => [props.visible, props.focusServerId] as const,
  ([open, focusId]) => {
    if (!open || !focusId) return;
    // 先拉最新列表：表单填充需要完整行数据，且定位目标可能在他处刚被改
    // 过。找不到（已删除）时落到 mcp 列表视图兜底。
    void fetchMcpServers().then(() => {
      const server = mcpServers.value.find((item) => item.id === focusId);
      if (server) {
        // 列表作用域跟到该 server 的归属，从编辑表单返回列表时看得到它
        scopeId.value = server.project_id ?? GLOBAL_SCOPE;
        view.value = { name: "mcp-edit", id: server.id };
      } else {
        view.value = { name: "mcp" };
      }
    });
  },
);

/**
 * 编辑表单的重建键：切换编辑目标（含 新增 ↔ 编辑）时强制新表单实例，
 * 字段不跨目标携带。同目标的重复进入（深链等）也走新实例。
 */
const editFormKey = computed(() => {
  const current = view.value;
  return current.name === "mcp-edit" ? `edit-${current.id ?? "new"}` : "";
});

function handleUpdateOpen(open: boolean) {
  if (!open) emit("close");
}
</script>
