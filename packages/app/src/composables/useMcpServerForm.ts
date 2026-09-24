import { computed, ref } from "vue";
import { useMcpServers } from "@/composables/useMcpServers";
import type { components } from "@/api/schema";

type McpServerResponse = components["schemas"]["McpServerResponse"];
type CreateMcpServerRequest = components["schemas"]["CreateMcpServerRequest"];
type UpdateMcpServerRequest = components["schemas"]["UpdateMcpServerRequest"];

/**
 * MCP 编辑表单的序列化/校验/提交，集中在此供 McpEditPage 使用（表单状态
 * 归子页所有：每次进入表单都是一个新的 composable 实例，离开即弃）。
 *
 * 行 ↔ 表单的序列化规则：
 * - args 一行一个命令行参数；env / headers 一行一个 KEY=value；
 * - toolTimeout 留空 = 不设置（create）/ 保持原值（update）；
 * - projectId 用作用域哨兵（GLOBAL_SCOPE）表达「全局」。
 */
export function useMcpServerForm(options: {
  /** 作用域 select 的全局哨兵值（与设置壳共享的常量约定）。 */
  globalScopeValue: string;
  /** 新建时的默认作用域（项目作用域下新增默认归属该项目）。 */
  defaultProjectId?: string | null;
}) {
  const { globalScopeValue, defaultProjectId } = options;
  const {
    servers,
    error: mcpError,
    clearError: clearMcpError,
    createServer,
    updateServer,
  } = useMcpServers();

  const form = ref(emptyForm());
  const error = ref<string | null>(null);
  const submitting = ref(false);

  function emptyForm() {
    return {
      name: "",
      /** "stdio" | "http"，决定表单展示哪组字段、提交带哪些键 */
      transport: "stdio",
      command: "",
      /** 一行一个命令行参数 */
      args: "",
      /** 一行一个 KEY=value */
      env: "",
      /** http 端点 */
      url: "",
      /** 一行一个 KEY=value（http 请求头，Bearer 认证入口） */
      headers: "",
      /** 空 = 不设置（create）/ 保持原值（update） */
      toolTimeout: "",
      projectId: defaultProjectId ?? globalScopeValue,
    };
  }

  /** 用现有行填充表单（编辑入口 / 深链定位）。 */
  function fillFrom(server: McpServerResponse) {
    form.value = {
      name: server.name,
      transport: server.transport === "http" ? "http" : "stdio",
      command: server.command ?? "",
      args: (server.args ?? []).join("\n"),
      env: Object.entries(server.env ?? {})
        .map(([key, value]) => `${key}=${value}`)
        .join("\n"),
      url: server.url ?? "",
      headers: Object.entries(server.headers ?? {})
        .map(([key, value]) => `${key}=${value}`)
        .join("\n"),
      toolTimeout:
        server.tool_timeout_ms != null ? String(server.tool_timeout_ms) : "",
      projectId: server.project_id ?? globalScopeValue,
    };
    error.value = null;
  }

  function parseArgsLines(text: string): string[] {
    return text
      .split("\n")
      .map((line) => line.trim())
      .filter((line) => line.length > 0);
  }

  /** KEY=value 行 → 对象；与 parseEnvLines 同构（env 与 http headers 共用）。 */
  function parseKeyValueLines(text: string): Record<string, string> {
    const map: Record<string, string> = {};
    for (const line of text.split("\n")) {
      const trimmed = line.trim();
      if (!trimmed) continue;
      const eq = trimmed.indexOf("=");
      const key = eq === -1 ? trimmed : trimmed.slice(0, eq).trim();
      // 格式非法的行在 canSubmit 已拦截；这里防御性跳过
      if (eq !== -1 && key) map[key] = trimmed.slice(eq + 1);
    }
    return map;
  }

  /** KEY=value 行格式（env 与 headers 同规则）：空行或 KEY=value。 */
  const linesValid = (text: string) =>
    text.split("\n").every((line) => {
      const trimmed = line.trim();
      return !trimmed || trimmed.includes("=");
    });

  const canSubmit = computed(() => {
    if (form.value.name.trim().length === 0) return false;
    // 必填字段随 transport：stdio 要 command，http 要 url
    if (form.value.transport === "http") {
      if (form.value.url.trim().length === 0) return false;
    } else if (form.value.command.trim().length === 0) {
      return false;
    }
    return linesValid(form.value.env) && linesValid(form.value.headers);
  });

  /**
   * 提交（create 或 update，按 serverId 区分）。成功返回 true（由页面
   * 负责「回到列表」的导航）；失败把文案接进表单自己的 error（列表级
   * 的 mcpError 单例同接管后清除，不残留到列表视图顶部）。
   */
  async function submit(serverId: string | null): Promise<boolean> {
    if (!canSubmit.value || submitting.value) return false;
    error.value = null;

    let toolTimeout: number | null = null;
    if (form.value.toolTimeout.trim()) {
      toolTimeout = Number.parseInt(form.value.toolTimeout, 10);
      if (!Number.isFinite(toolTimeout) || toolTimeout <= 0) {
        error.value = "tool_timeout_ms 需为正整数（毫秒）。";
        return false;
      }
    }

    const args = parseArgsLines(form.value.args);
    const env = parseKeyValueLines(form.value.env);
    const headers = parseKeyValueLines(form.value.headers);
    const projectId =
      form.value.projectId === globalScopeValue ? null : form.value.projectId;
    const name = form.value.name.trim();
    const transport = form.value.transport;
    const command = form.value.command.trim();
    const url = form.value.url.trim();

    submitting.value = true;
    try {
      if (serverId) {
        // 编辑：所见即所得——只提交当前 transport 的字段组（另一组的存量
        // 由后端保留，transport 来回切不丢配置）。空 args/env/headers 传空
        // 集合清空；timeout/作用域留空则省略字段保持原值（后端 PATCH
        // 语义：提供即替换、缺省保持）。
        const ok = await updateServer(serverId, {
          name,
          transport,
          ...(transport === "http" ? { url, headers } : { command, args, env }),
          ...(toolTimeout != null ? { tool_timeout_ms: toolTimeout } : {}),
          ...(projectId ? { project_id: projectId } : {}),
        } satisfies UpdateMcpServerRequest);
        if (!ok) {
          error.value = mcpError.value ?? "更新 MCP server 失败";
          clearMcpError();
          return false;
        }
        return true;
      }
      const created = await createServer({
        name,
        transport,
        ...(transport === "http"
          ? {
              url,
              ...(Object.keys(headers).length > 0 ? { headers } : {}),
            }
          : {
              command,
              ...(args.length > 0 ? { args } : {}),
              ...(Object.keys(env).length > 0 ? { env } : {}),
            }),
        ...(toolTimeout != null ? { tool_timeout_ms: toolTimeout } : {}),
        ...(projectId ? { project_id: projectId } : {}),
      } satisfies CreateMcpServerRequest);
      if (!created) {
        error.value = mcpError.value ?? "创建 MCP server 失败";
        clearMcpError();
        return false;
      }
      return true;
    } finally {
      submitting.value = false;
    }
  }

  return {
    servers,
    form,
    error,
    submitting,
    canSubmit,
    fillFrom,
    submit,
  };
}
