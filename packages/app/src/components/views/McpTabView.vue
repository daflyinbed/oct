<template>
  <section class="workspace-panel min-w-0 flex-1 bg-background">
    <header
      class="workspace-panel-header gap-2 text-[12.5px] text-neutral-10 font-semibold"
    >
      <i-lucide-plug class="h-3.5 w-3.5 flex-none text-dim" />
      <span class="min-w-0 truncate">{{ server?.name ?? serverId }}</span>
      <span
        v-if="server?.command"
        class="max-w-[40%] truncate rounded bg-neutral-2 px-1.5 py-px text-[10.5px] text-dim leading-[16px] font-mono"
      >
        {{ server.command }}
      </span>
      <span class="flex-1" />
      <span
        v-if="server"
        class="flex flex-none items-center gap-1.5 text-[11.5px]"
        :title="mcpStatusText(server)"
      >
        <span
          class="h-1.5 w-1.5 rounded-full"
          :class="mcpStatusDot(server.status)"
        />
        <span :class="mcpStatusClass(server.status)">{{
          mcpStatusShortText(server)
        }}</span>
      </span>
      <button
        class="h-[24px] inline-flex flex-none items-center gap-1.5 control-surface rounded-md px-2 text-[11.5px] font-medium control-focus disabled:opacity-50"
        :disabled="ensuring || !activeProjectId"
        title="重新对账并刷新"
        @click="refresh"
      >
        <i-lucide-loader-circle v-if="ensuring" class="h-3 w-3 animate-spin" />
        <i-lucide-rotate-ccw v-else class="h-3 w-3" />
        刷新
      </button>
    </header>

    <!-- 每个 server 独占一个面板：工具之外的能力（资源、Prompts 等）后续
         作为独立分区接入同一面板 -->
    <div v-if="server" class="workspace-panel-body p-4">
      <!-- 连接失败：错误日志即面板主体 -->
      <template v-if="server.status === 'failed'">
        <h3
          class="mb-2 text-[10.5px] text-faint font-medium tracking-wide uppercase"
        >
          最近错误
        </h3>
        <pre
          class="whitespace-pre-wrap border border-danger-6 rounded-md bg-danger-1 px-2.5 py-2 text-[11.5px] text-danger-10 font-mono"
          >{{ server.error ?? "未知错误" }}</pre
        >
      </template>

      <!-- 已连接：加载到的工具清单（原始服务端名与描述） -->
      <template v-else-if="server.status === 'connected'">
        <h3
          class="mb-2 text-[10.5px] text-faint font-medium tracking-wide uppercase"
        >
          工具 · {{ server.tools?.length ?? 0 }}
        </h3>
        <ul v-if="server.tools?.length" class="flex flex-col gap-1.5">
          <li
            v-for="tool in server.tools"
            :key="tool.name"
            class="flex flex-col gap-0.5 border border-line-soft rounded-md bg-surface px-2.5 py-1.5"
          >
            <code class="text-[12px] text-strong font-mono">{{
              tool.name
            }}</code>
            <span v-if="tool.description" class="text-[11.5px] text-dim">{{
              tool.description
            }}</span>
          </li>
        </ul>
        <p v-else class="text-[12px] text-faint">该 server 未上报任何工具。</p>
      </template>

      <p v-else class="text-[12px] text-faint">
        尚未连接；刷新或发送消息后会尝试建连。
      </p>
    </div>

    <div
      v-else
      class="flex flex-1 flex-col items-center justify-center gap-2 px-6 text-center text-faint"
    >
      <i-lucide-plug class="h-8 w-8" />
      <p class="text-[13px] text-dim">
        该 server 不在当前项目的有效集合中（可能已被删除或禁用）。
      </p>
    </div>
  </section>
</template>

<script setup lang="ts">
import { computed, watch } from "vue";
import {
  mcpStatusClass,
  mcpStatusDot,
  mcpStatusShortText,
  mcpStatusText,
  useMcpStatus,
} from "@/composables/useMcpStatus";
import type { components } from "@/api/schema";

type McpServerResponse = components["schemas"]["McpServerResponse"];

const props = defineProps<{
  /** 面板对应的 MCP server id（tab payload）。 */
  serverId: string;
  activeProjectId: string | null;
}>();

const { servers, ensuring, ensureServers } = useMcpStatus();

// 挂载/切换项目即对账刷新一次；面板数据（状态 + tools + error）全部来自
// ensure 响应，没有单独的拉取通道。
watch(
  () => props.activeProjectId,
  (projectId) => {
    if (projectId) void ensureServers(projectId);
  },
  { immediate: true },
);

function refresh() {
  if (props.activeProjectId) void ensureServers(props.activeProjectId);
}

const server = computed<McpServerResponse | null>(
  () => servers.value.find((item) => item.id === props.serverId) ?? null,
);
</script>
