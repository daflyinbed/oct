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

    <!-- 每个 server 独占一个面板：工具 / Prompts / Resources 三个分区 -->
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
          >{{ server.error ?? "未知错误" }}</pre>
      </template>

      <!-- 已连接：加载到的工具清单（原始服务端名与描述） -->
      <template v-else-if="server.status === 'connected'">
        <h3
          class="mb-2 text-[10.5px] text-faint font-medium tracking-wide uppercase"
        >
          工具 · {{ server.tools?.length ?? 0 }}
        </h3>
        <ul v-if="server.tools?.length" class="mb-5 flex flex-col gap-1.5">
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
        <p v-else class="mb-5 text-[12px] text-faint">
          该 server 未上报任何工具。
        </p>

        <!-- Prompts：只读清单,使用入口是聊天输入框的斜杠命令 -->
        <h3
          class="mb-2 text-[10.5px] text-faint font-medium tracking-wide uppercase"
        >
          Prompts · {{ server.prompts?.length ?? 0 }}
        </h3>
        <template v-if="server.prompts?.length">
          <p class="mb-2 text-[11.5px] text-faint">
            在聊天输入框输入 <code class="font-mono">/</code> 选择使用。
          </p>
          <ul class="mb-5 flex flex-col gap-1.5">
            <li
              v-for="prompt in server.prompts"
              :key="prompt.name"
              class="flex flex-col gap-0.5 border border-line-soft rounded-md bg-surface px-2.5 py-1.5"
            >
              <span class="flex items-center gap-1.5">
                <code class="text-[12px] text-strong font-mono"
                  >/{{ prompt.name }}</code
                >
                <span
                  v-if="prompt.arguments.length > 0"
                  class="text-[10.5px] text-faint"
                  >{{ prompt.arguments.length }} 个参数</span
                >
              </span>
              <span v-if="prompt.description" class="text-[11.5px] text-dim">{{
                prompt.description
              }}</span>
              <span
                v-for="argument in prompt.arguments"
                :key="argument.name"
                class="text-[11px] text-faint"
              >
                <code class="font-mono">{{ argument.name }}</code>
                <span v-if="argument.required" class="text-danger-10">*</span>
                <span v-if="argument.description" class="ml-1">{{
                  argument.description
                }}</span>
              </span>
            </li>
          </ul>
        </template>
        <p v-else class="mb-5 text-[12px] text-faint">
          该 server 未上报任何 prompt。
        </p>

        <!-- Resources：只读清单 + 面板内查看（不进 chat/模型） -->
        <h3
          class="mb-2 text-[10.5px] text-faint font-medium tracking-wide uppercase"
        >
          Resources · {{ server.resources?.length ?? 0 }}
        </h3>
        <ul v-if="server.resources?.length" class="flex flex-col gap-1.5">
          <li
            v-for="resource in server.resources"
            :key="resource.uri"
            class="flex flex-col gap-0.5 border border-line-soft rounded-md bg-surface px-2.5 py-1.5"
          >
            <button
              type="button"
              class="flex items-center gap-1.5 rounded-[4px] text-left control-focus"
              :title="`读取 ${resource.uri}`"
              @click="toggleResource(resource)"
            >
              <i-lucide-chevron-right
                class="h-3 w-3 flex-none text-faint transition-transform"
                :class="openResource?.uri === resource.uri ? 'rotate-90' : ''"
              />
              <span class="min-w-0 flex-1 truncate text-[12px] text-strong">
                {{ resource.name }}
              </span>
              <span
                v-if="resource.mime_type"
                class="flex-none text-[10.5px] text-faint font-mono"
                >{{ resource.mime_type }}</span
              >
            </button>
            <span
              v-if="resource.description"
              class="pl-[24px] text-[11.5px] text-dim"
              >{{ resource.description }}</span
            >
            <code
              class="truncate pl-[24px] text-[10.5px] text-faint font-mono"
              >{{ resource.uri }}</code
            >

            <!-- 内容查看：文本原样,二进制给占位说明 -->
            <div
              v-if="openResource?.uri === resource.uri"
              class="ml-[24px] mt-1 border-l-2 border-line-soft pl-2.5"
            >
              <p
                v-if="resourceError"
                class="whitespace-pre-wrap text-[11.5px] text-danger-10"
              >
                {{ resourceError }}
              </p>
              <i-lucide-loader-circle
                v-else-if="resourceLoading"
                class="h-3.5 w-3.5 animate-spin text-faint"
              />
              <pre
                v-else-if="resourceContent?.text != null"
                class="max-h-[320px] overflow-auto whitespace-pre-wrap rounded-md bg-neutral-1 px-2.5 py-2 text-[11.5px] text-neutral-10 font-mono"
                >{{ resourceContent.text }}</pre>
              <p
                v-else-if="resourceContent?.blob_base64 != null"
                class="text-[11.5px] text-faint"
              >
                二进制内容（{{
                  resourceContent.mime_type ?? resource.mime_type ?? "未知类型"
                }}，base64
                {{ resourceContent.blob_base64.length }} 字符），暂不支持预览。
              </p>
            </div>
          </li>
        </ul>
        <p v-else class="text-[12px] text-faint">
          该 server 未上报任何 resource。
        </p>
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
import { computed, ref, watch } from "vue";
import { readMcpResource } from "@/composables/useMcpServers";
import {
  mcpStatusClass,
  mcpStatusDot,
  mcpStatusShortText,
  mcpStatusText,
  useMcpStatus,
} from "@/composables/useMcpStatus";
import type { components } from "@/api/schema";
import type { McpResourceContent } from "@/composables/useMcpServers";

type McpServerResponse = components["schemas"]["McpServerResponse"];
type McpResourceSummary = components["schemas"]["McpResourceSummary"];

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

// --- Resources 面板内查看（逐条惰性读取,同屏只展开一条） -------------------

const openResource = ref<McpResourceSummary | null>(null);
const resourceContent = ref<McpResourceContent | null>(null);
const resourceLoading = ref(false);
const resourceError = ref<string | null>(null);

async function toggleResource(resource: McpResourceSummary) {
  if (openResource.value?.uri === resource.uri) {
    openResource.value = null;
    resourceContent.value = null;
    resourceError.value = null;
    return;
  }
  openResource.value = resource;
  resourceContent.value = null;
  resourceError.value = null;
  resourceLoading.value = true;
  try {
    resourceContent.value = await readMcpResource(props.serverId, resource.uri);
  } catch (error) {
    resourceError.value =
      error instanceof Error ? error.message : String(error);
  } finally {
    resourceLoading.value = false;
  }
}
</script>
