import { computed, ref } from "vue";
import client from "@/api/client";
import type { components } from "@/api/schema";

export type AgentView = components["schemas"]["AgentSettingsView"];
export type ToolSelector = components["schemas"]["ToolSelector"];
type AgentPatch = components["schemas"]["AgentPatch"];
export type ToolOption = components["schemas"]["AgentToolOption"];
type Provider = components["schemas"]["ProviderResponse"];

export function toolKey(tool: ToolSelector): string {
  return JSON.stringify(
    tool.kind === "builtin"
      ? ["builtin", tool.name]
      : ["mcp", tool.server_id, tool.tool],
  );
}

/** Per-editor state: scope/request ownership guards both reads and save replies. */
export function useAgentSettings() {
  const views = ref<AgentView[]>([]);
  const tools = ref<ToolOption[]>([]);
  const providers = ref<Provider[]>([]);
  const loading = ref(false);
  const saving = ref(false);
  const error = ref("");
  const saved = ref("");
  let generation = 0;
  let project: string | null = null;

  async function load(projectId: string | null) {
    const owner = ++generation;
    project = projectId;
    loading.value = true;
    saving.value = false;
    error.value = "";
    saved.value = "";
    views.value = [];
    tools.value = [];
    try {
      const [settings, catalog, directory] = await Promise.all([
        projectId
          ? client.GET("/api/projects/{id}/settings/agents", {
              params: { path: { id: projectId } },
            })
          : client.GET("/api/settings/agents"),
        projectId
          ? client.GET("/api/projects/{id}/settings/agent-tools", {
              params: { path: { id: projectId } },
            })
          : client.GET("/api/settings/agent-tools"),
        client.GET("/api/providers"),
      ]);
      if (owner !== generation) return;
      if (settings.error || catalog.error || directory.error) {
        error.value =
          settings.error?.error ?? catalog.error?.error ?? "加载配置失败";
        return;
      }
      views.value = settings.data!;
      tools.value = catalog.data!;
      providers.value = directory.data!;
    } catch (error_) {
      if (owner === generation)
        error.value = error_ instanceof Error ? error_.message : "加载配置失败";
    } finally {
      if (owner === generation) loading.value = false;
    }
  }

  async function save(key: string, body: AgentPatch): Promise<boolean> {
    const owner = generation;
    const target = project;
    saving.value = true;
    error.value = "";
    saved.value = "";
    try {
      const response = target
        ? await client.PATCH("/api/projects/{id}/settings/agents/{key}", {
            params: { path: { id: target, key } },
            body,
          })
        : await client.PATCH("/api/settings/agents/{key}", {
            params: { path: { key } },
            body,
          });
      if (owner !== generation) return false;
      if (response.error) {
        error.value = response.error.error;
        return false;
      }
      views.value = views.value.map((v) =>
        v.key === key ? response.data! : v,
      );
      saved.value =
        key === "title"
          ? "已保存，对之后触发的标题生成生效；已有标题不会重新生成。"
          : "已保存，对之后新建的子任务生效。";
      return true;
    } catch (error_) {
      if (owner === generation)
        error.value = error_ instanceof Error ? error_.message : "保存配置失败";
      return false;
    } finally {
      if (owner === generation) saving.value = false;
    }
  }

  const modelOptions = computed(() =>
    providers.value.flatMap((p) =>
      p.models.map((m) => ({
        value: JSON.stringify([p.id, m.model_id]),
        label: `${p.name} / ${m.name}${m.is_enabled ? "" : "（聊天列表隐藏）"}`,
      })),
    ),
  );
  function dispose() {
    generation++;
  }
  return {
    views,
    tools,
    loading,
    saving,
    error,
    saved,
    modelOptions,
    load,
    save,
    dispose,
  };
}
