<template>
  <DialogTitle class="text-[16.5px] text-strong font-semibold">
    MCP Servers
  </DialogTitle>
  <p class="mt-1 text-[12px] text-dim">
    为 Agent 接入外部工具服务器（stdio / http）；启用后下一次对话开始时连接。{{
      inProjectScope
        ? `此处为项目「${activeScopeProjectName}」专属配置，全局配置见「全局」作用域。`
        : "此处为全局作用域，对所有项目生效。"
    }}
  </p>

  <p v-if="mcpError" class="mt-3 text-[12px] text-danger-10">
    {{ mcpError }}
  </p>

  <!-- enabled=false 整行弱化（含状态点/徽标/开关），开关仍可用 -->
  <div
    v-for="server in filteredServers"
    :key="server.id"
    class="border-b border-line-soft border-dashed py-3 transition-opacity"
    :class="server.enabled ? '' : 'opacity-60'"
  >
    <div class="flex items-center gap-2">
      <span
        class="h-1.5 w-1.5 flex-none rounded-full"
        :class="mcpStatusDot(server.status)"
        :title="mcpStatusText(server)"
      />
      <span class="text-[13px] text-strong font-semibold">{{
        server.name
      }}</span>
      <span
        class="rounded bg-neutral-2 px-1.5 py-px text-[10.5px] text-dim font-medium leading-[16px]"
      >
        {{ scopeLabel(server) }}
      </span>
      <span
        class="rounded bg-neutral-2 px-1.5 py-px text-[10.5px] text-dim font-medium leading-[16px]"
      >
        {{ server.transport }}
      </span>
      <span class="flex-1" />
      <button
        class="relative h-[19px] w-[34px] flex-none rounded-full transition-colors"
        :class="server.enabled ? 'bg-accent-7' : 'bg-neutral-2'"
        role="switch"
        :aria-checked="server.enabled"
        :aria-label="`启用 ${server.name}`"
        :title="server.enabled ? '禁用该 server' : '启用该 server'"
        @click="toggleEnabled(server)"
      >
        <span
          class="absolute left-[2px] top-[2px] h-[15px] w-[15px] rounded-full bg-white shadow-[0_1px_3px_rgba(0,0,0,0.3)] transition-transform"
          :class="server.enabled ? 'translate-x-[15px]' : ''"
        />
      </button>
      <button
        class="h-[26px] inline-flex flex-none items-center gap-1.5 control-surface rounded-md px-2.5 text-[12px] font-medium control-focus"
        :title="`配置 ${server.name}`"
        @click="emit('edit', server)"
      >
        Configure
        <i-lucide-chevron-right class="h-3 w-3 text-dim" />
      </button>
      <!-- 删除是破坏性操作，hover 反馈比普通 ghost（neutral-1，亮色下 ~5%
           黑底几乎不可见）强一档：提到 neutral-3，与 Configure
           （control-surface）的 hover 同级，亮暗色都可感知 -->
      <button
        class="h-[26px] w-[26px] flex-none rounded-md text-dim control-focus transition-colors active:bg-neutral-4 hover:bg-neutral-3 hover:text-neutral-10"
        :title="`删除 ${server.name}`"
        @click="removeServer(server)"
      >
        <i-lucide-trash-2 class="h-3.5 w-3.5" />
      </button>
    </div>
    <!-- 状态/错误副文本独占一行（长错误截断，完整文本悬停状态点的 title
         可见）；stdio 行显示 command、http 行显示 url，用 text-dim 保证亮色
         下可读（text-faint 在亮色面板上对比度不足） -->
    <div class="mt-1.5 pl-3.5">
      <span
        class="block truncate text-[12px]"
        :class="mcpStatusClass(server.status)"
      >
        {{ mcpStatusText(server) }}
      </span>
      <span
        v-if="server.command || server.url"
        class="mt-0.5 block truncate text-[11.5px] text-dim font-mono"
        :title="server.command ?? server.url ?? ''"
      >
        {{ server.command ?? server.url }}
      </span>
    </div>
  </div>

  <div
    v-if="filteredServers.length === 0 && !mcpLoading"
    class="py-10 text-center"
  >
    <p class="text-[12.5px] text-faint">{{ emptyText }}</p>
    <button
      class="mt-3 h-[26px] inline-flex items-center gap-1.5 control-surface rounded-md px-2.5 text-[12px] font-medium control-focus"
      @click="emit('add')"
    >
      <i-lucide-plus class="h-3 w-3" />
      Add Server
    </button>
  </div>
  <p v-if="mcpLoading" class="py-8 text-center text-[12.5px] text-faint">
    Loading…
  </p>
</template>

<script setup lang="ts">
import { DialogTitle } from "reka-ui";
import { computed } from "vue";
import { useMcpServers } from "@/composables/useMcpServers";
import {
  mcpStatusClass,
  mcpStatusDot,
  mcpStatusText,
} from "@/composables/useMcpStatus";
import { useProjects } from "@/composables/useProjects";
import type { components } from "@/api/schema";

type McpServerResponse = components["schemas"]["McpServerResponse"];

const props = defineProps<{
  /** 搜索框关键词（壳层拥有，过滤在各子页做）。 */
  query: string;
  /** 当前是否项目作用域（壳层的 scope 派生值）。 */
  inProjectScope: boolean;
  /** 项目作用域下的项目名（文案用）。 */
  activeScopeProjectName: string;
  /** 作用域 id：全局哨兵或项目 id，列表按它过滤。 */
  scopeId: string;
}>();

const emit = defineEmits<{
  add: [];
  edit: [server: McpServerResponse];
}>();

const {
  servers: mcpServers,
  loading: mcpLoading,
  error: mcpError,
  deleteServer,
  toggleEnabled: toggleMcpEnabled,
} = useMcpServers();

const { projects } = useProjects();

/** 当前作用域可见的 servers：全局 = project_id 为空；项目 = 归属所选项目。 */
const scopedServers = computed(() => {
  if (!props.inProjectScope) {
    return mcpServers.value.filter((server) => !server.project_id);
  }
  return mcpServers.value.filter(
    (server) => server.project_id === props.scopeId,
  );
});

const filteredServers = computed(() => {
  const q = props.query.trim().toLowerCase();
  return scopedServers.value.filter(
    (server) => !q || server.name.toLowerCase().includes(q),
  );
});

const emptyText = computed(() => {
  if (props.inProjectScope) {
    const name = props.activeScopeProjectName;
    return name
      ? `项目「${name}」下尚未配置任何 MCP server。`
      : "尚未配置任何 MCP server。";
  }
  return "全局作用域下尚未配置任何 MCP server。";
});

function scopeLabel(server: McpServerResponse): string {
  if (!server.project_id) return "全局";
  return (
    projects.value.find((p) => p.id === server.project_id)?.name ?? "未知项目"
  );
}

function toggleEnabled(server: McpServerResponse) {
  void toggleMcpEnabled(server, !server.enabled);
}

function removeServer(server: McpServerResponse) {
  void deleteServer(server.id);
}
</script>
