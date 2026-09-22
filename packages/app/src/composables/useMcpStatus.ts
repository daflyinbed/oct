import { readonly, ref, shallowReadonly } from "vue";
import client from "@/api/client";
import type { components } from "@/api/schema";

type McpServerResponse = components["schemas"]["McpServerResponse"];

/** 当前项目有效集合（全局 + 项目级，仅 enabled）的实时状态，供状态栏指示器使用。 */
const servers = ref<McpServerResponse[]>([]);
const ensuring = ref(false);

/**
 * 请求序号：ensure 与项目切换可能并发（连点会话/开 popover），只有最新一次
 * 调用的响应允许写入状态，避免慢响应把旧项目的列表覆盖到新项目头上。
 */
let ensureSeq = 0;

export function useMcpStatus() {
  /**
   * 对账并拉取 projectId 的有效 MCP 集合：后端先 reconcile（连接缺失/变更
   * 的 server、断开已删除/禁用的），再返回带实时 status 的列表。幂等，已
   * 连接且配置未变的 server 直接复用。失败（如 404）保留旧列表静默返回。
   */
  const ensureServers = async (projectId: string) => {
    const seq = ++ensureSeq;
    ensuring.value = true;
    try {
      const { data } = await client.POST(
        "/api/projects/{projectId}/mcp/ensure",
        {
          params: { path: { projectId } },
        },
      );
      if (data && seq === ensureSeq) {
        servers.value = data;
      }
    } finally {
      if (seq === ensureSeq) {
        ensuring.value = false;
      }
    }
  };

  return {
    // shallowReadonly：与 useMcpServers 同理，args/env 是对象/数组字段
    servers: shallowReadonly(servers),
    ensuring: readonly(ensuring),
    ensureServers,
  };
}

// ---------------------------------------------------------------------------
// 状态展示（SettingsModal 与状态栏指示器共用）
// ---------------------------------------------------------------------------

export function mcpStatusDot(status: string): string {
  return (
    {
      connected: "bg-success-10",
      failed: "bg-danger-10",
      not_connected: "bg-neutral-5",
    }[status] ?? "bg-neutral-5"
  );
}

export function mcpStatusText(server: McpServerResponse): string {
  if (server.status === "connected") {
    return server.tool_count != null
      ? `已连接 · ${server.tool_count} 个工具`
      : "已连接";
  }
  if (server.status === "failed") {
    return server.error ?? "连接失败";
  }
  return "未连接";
}

export function mcpStatusClass(status: string): string {
  if (status === "connected") return "text-dim";
  if (status === "failed") return "text-danger-10";
  return "text-faint";
}

/** 行尾短文案；完整状态（含失败原因）走 mcpStatusText。 */
export function mcpStatusShortText(server: McpServerResponse): string {
  if (server.status === "connected") {
    return server.tool_count != null ? `${server.tool_count} 个工具` : "已连接";
  }
  if (server.status === "failed") return "连接失败";
  return "未连接";
}

// ---------------------------------------------------------------------------
// 作用域分组（状态栏子菜单与工具面板共用）
// ---------------------------------------------------------------------------

export interface McpServerGroup {
  label: string;
  servers: McpServerResponse[];
}

/**
 * 有效集合按作用域分组：全局（NULL scope，服务所有项目）在上，当前项目
 * 专属在下。组标签用项目名；空组直接剔除。有效集合里的项目级行必属当前
 * 项目（后端按项目过滤），所以按 project_id 是否为空分桶即可。
 */
export function groupMcpServersByScope(
  servers: readonly McpServerResponse[],
  projectName: string | null,
): McpServerGroup[] {
  return [
    { label: "全局", servers: servers.filter((s) => !s.project_id) },
    {
      label: projectName ?? "本项目",
      servers: servers.filter((s) => s.project_id),
    },
  ].filter((group) => group.servers.length > 0);
}
