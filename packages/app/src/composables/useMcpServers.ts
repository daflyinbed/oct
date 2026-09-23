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

// ---------------------------------------------------------------------------
// prompts 渲染 / resources 读取（斜杠菜单与 MCP 面板的两个瞬时调用，无
// 本地状态，独立于上面的 CRUD 单例）
// ---------------------------------------------------------------------------

export type McpPromptRenderResult =
  components["schemas"]["RenderPromptResponse"];
export type McpResourceContent = components["schemas"]["ReadResourceResponse"];

/**
 * 渲染（prompts/get）一个 MCP prompt：`arguments` 为参数名到字符串值的
 * 映射，无参 prompt 传空对象。成功返回 messages；失败（404/502 等）抛出
 * 携带后端 error 文案的 Error。
 */
export async function renderMcpPrompt(
  serverId: string,
  name: string,
  // 形参不能叫 arguments(ESM 严格模式保留字),请求体字段名保持 arguments。
  argumentValues: Record<string, string>,
): Promise<McpPromptRenderResult> {
  const { data, error } = await client.POST(
    "/api/mcp/servers/{id}/prompts/{name}",
    {
      params: { path: { id: serverId, name } },
      body: { arguments: argumentValues },
    },
  );
  if (error || !data) {
    throw new Error(toErrorMessage(error, `渲染 prompt \`${name}\` 失败`));
  }
  return data;
}

/** 读取（resources/read）一个 MCP resource 的内容（文本或 base64 blob）。 */
export async function readMcpResource(
  serverId: string,
  uri: string,
): Promise<McpResourceContent> {
  const { data, error } = await client.POST(
    "/api/mcp/servers/{id}/resources/read",
    {
      params: { path: { id: serverId } },
      body: { uri },
    },
  );
  if (error || !data) {
    throw new Error(toErrorMessage(error, `读取 resource \`${uri}\` 失败`));
  }
  return data;
}
