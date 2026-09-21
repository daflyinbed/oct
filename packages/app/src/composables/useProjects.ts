import { readonly, ref } from "vue";
import client from "@/api/client";
import type { components } from "@/api/schema";

type Project = components["schemas"]["Project"];
type Conversation = components["schemas"]["Conversation"];

const projects = ref<Project[]>([]);
const conversations = ref<Map<string, Conversation[]>>(new Map());
const loading = ref(false);

export function useProjects() {
  const fetchConversations = async (projectId: string) => {
    const { data, error } = await client.GET(
      "/api/projects/{projectId}/conversations",
      { params: { path: { projectId } } },
    );
    if (!error && data) {
      conversations.value = new Map(conversations.value).set(projectId, data);
    }
  };

  const fetchProjects = async () => {
    loading.value = true;
    const { data, error } = await client.GET("/api/projects");
    if (!error && data) {
      projects.value = data;
      for (const project of data) {
        await fetchConversations(project.id);
      }
    }
    loading.value = false;
  };

  const createProject = async (name: string, workingDir: string) => {
    const { data, error } = await client.POST("/api/projects", {
      body: { name, working_dir: workingDir },
    });
    if (!error && data) {
      projects.value = [...projects.value, data];
      conversations.value = new Map(conversations.value).set(data.id, []);
      return data;
    }
    return null;
  };

  const createConversation = async (
    projectId: string,
    title?: string | null,
  ) => {
    const { data, error } = await client.POST(
      "/api/projects/{projectId}/conversations",
      {
        params: { path: { projectId } },
        body: { title: title ?? null },
      },
    );
    if (!error && data) {
      const existing = conversations.value.get(projectId) ?? [];
      conversations.value = new Map(conversations.value).set(projectId, [
        ...existing,
        data,
      ]);
      return data;
    }
    return null;
  };

  const deleteConversation = async (projectId: string, id: string) => {
    const { error } = await client.DELETE(
      "/api/projects/{projectId}/conversations/{id}",
      { params: { path: { projectId, id } } },
    );
    if (!error) {
      const existing = conversations.value.get(projectId) ?? [];
      conversations.value = new Map(conversations.value).set(
        projectId,
        existing.filter((c) => c.id !== id),
      );
    }
  };

  /**
   * 本地更新某个会话的标题（title_updated 事件到达时）。后端保证手动命名
   * 永不被自动标题覆盖，所以这里无条件覆盖即可。不命中任何列表则忽略——
   * 该会话尚未在侧栏加载，下次 fetchConversations 自然带上新标题。
   */
  const updateConversationTitle = (conversationId: string, title: string) => {
    for (const [projectId, list] of conversations.value) {
      const index = list.findIndex((c) => c.id === conversationId);
      if (index !== -1) {
        const next = [...list];
        // title_source 一并补上：来源字段的价值在数据里，不留"default 配 AI
        // 标题"的矛盾状态（当前无读取方，但保持与后端落库结果一致）。
        next[index] = { ...next[index]!, title, title_source: "ai" };
        conversations.value = new Map(conversations.value).set(projectId, next);
        return;
      }
    }
  };

  return {
    projects: readonly(projects),
    conversations: readonly(conversations),
    loading: readonly(loading),
    fetchProjects,
    fetchConversations,
    createProject,
    createConversation,
    deleteConversation,
    updateConversationTitle,
  };
}
