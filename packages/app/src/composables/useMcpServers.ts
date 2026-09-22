import { readonly, ref, shallowReadonly } from "vue";
import client from "@/api/client";
import type { components } from "@/api/schema";

type McpServerResponse = components["schemas"]["McpServerResponse"];
type CreateMcpServerRequest = components["schemas"]["CreateMcpServerRequest"];
type UpdateMcpServerRequest = components["schemas"]["UpdateMcpServerRequest"];

const servers = ref<McpServerResponse[]>([]);
const loading = ref(false);
const error = ref<string | null>(null);

/** 后端错误体是 `{ error: string }`（AppError）；schema 未声明其形状，运行时取出。 */
function toErrorMessage(err: unknown, fallback: string): string {
  const message = (err as { error?: unknown } | null | undefined)?.error;
  return typeof message === "string" && message ? message : fallback;
}

export function useMcpServers() {
  const fetchServers = async (projectId?: string) => {
    loading.value = true;
    const { data } = await client.GET("/api/mcp/servers", {
      params: projectId ? { query: { project_id: projectId } } : undefined,
    });
    if (data) {
      servers.value = data;
    }
    loading.value = false;
  };

  /** 创建成功后把返回的行（status 为 not_connected）追加进本地列表。 */
  const createServer = async (input: CreateMcpServerRequest) => {
    error.value = null;
    const { data, error: err } = await client.POST("/api/mcp/servers", {
      body: input,
    });
    if (!err && data) {
      servers.value = [...servers.value, data];
      return data;
    }
    error.value = toErrorMessage(err, "创建 MCP server 失败");
    return null;
  };

  /**
   * PATCH 成功后重 GET 单个 server 以服务端落库结果就地替换本地条目
   * （后端的 PATCH 无响应体，且 status/tool_count 是实时派生值）。
   */
  const updateServer = async (id: string, patch: UpdateMcpServerRequest) => {
    error.value = null;
    const { error: err } = await client.PATCH("/api/mcp/servers/{id}", {
      params: { path: { id } },
      body: patch,
    });
    if (err) {
      error.value = toErrorMessage(err, "更新 MCP server 失败");
      return false;
    }
    const { data } = await client.GET("/api/mcp/servers/{id}", {
      params: { path: { id } },
    });
    if (data) {
      servers.value = servers.value.map((s) => (s.id === id ? data : s));
    }
    return true;
  };

  const deleteServer = async (id: string) => {
    error.value = null;
    const { error: err } = await client.DELETE("/api/mcp/servers/{id}", {
      params: { path: { id } },
    });
    if (err) {
      error.value = toErrorMessage(err, "删除 MCP server 失败");
      return false;
    }
    servers.value = servers.value.filter((s) => s.id !== id);
    return true;
  };

  /**
   * 清除变更错误：错误是模块级单例，表单视图把失败信息接管到自己局部的
   * 错误状态后调用，避免同一条错误继续残留在列表视图顶部。
   */
  const clearError = () => {
    error.value = null;
  };

  /** 开关 enabled；乐观更新，失败时回滚为提交前的值（不整表 refetch）。 */
  const toggleEnabled = async (server: McpServerResponse, enabled: boolean) => {
    const idx = servers.value.findIndex((s) => s.id === server.id);
    if (idx === -1) return false;
    servers.value = servers.value.map((s, i) =>
      i === idx ? { ...s, enabled } : s,
    );
    const ok = await updateServer(server.id, { enabled });
    if (!ok) {
      servers.value = servers.value.map((s, i) =>
        i === idx ? { ...s, enabled: server.enabled } : s,
      );
    }
    return ok;
  };

  return {
    // shallowReadonly：args/env 是数组/对象字段，DeepReadonly 会与表单用的
    // 可变副本类型不兼容（同 useProjects 的 projects 暴露方式）。
    servers: shallowReadonly(servers),
    loading: readonly(loading),
    error: readonly(error),
    fetchServers,
    createServer,
    updateServer,
    deleteServer,
    toggleEnabled,
    clearError,
  };
}
