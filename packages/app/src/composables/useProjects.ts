import { readonly, ref, shallowReadonly } from "vue";
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

  const createProject = async (
    name: string,
    workingDir: string,
    additionalDirs?: string[],
  ) => {
    const { data, error } = await client.POST("/api/projects", {
      body: {
        name,
        working_dir: workingDir,
        ...(additionalDirs ? { additional_dirs: additionalDirs } : {}),
      },
    });
    if (!error && data) {
      projects.value = [...projects.value, data];
      conversations.value = new Map(conversations.value).set(data.id, []);
      return data;
    }
    return null;
  };

  /**
   * 编辑项目（名称/主目录/附加目录）。PATCH 成功后重新 GET 单个项目来
   * 更新本地列表——服务端会对附加目录做归一化（去重、剔除与主目录相同
   * 的条目），以落库结果为准而不是本地拼接。
   */
  const updateProject = async (
    projectId: string,
    patch: {
      name?: string;
      working_dir?: string;
      additional_dirs?: string[];
    },
  ) => {
    const { error } = await client.PATCH("/api/projects/{id}", {
      params: { path: { id: projectId } },
      body: patch,
    });
    if (error) return false;
    const { data } = await client.GET("/api/projects/{id}", {
      params: { path: { id: projectId } },
    });
    if (data) {
      projects.value = projects.value.map((p) =>
        p.id === projectId ? data : p,
      );
    }
    return true;
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
    // shallowReadonly 而非 readonly：additional_dirs 是数组字段，DeepReadonly
    // 会把它变成 readonly string[]，与各组件 Project prop 的 string[] 不兼容。
    // 数组本身仍受只读守卫（不能整体替换/增删项目）。
    projects: shallowReadonly(projects),
    conversations: readonly(conversations),
    loading: readonly(loading),
    fetchProjects,
    fetchConversations,
    createProject,
    updateProject,
    createConversation,
    deleteConversation,
    updateConversationTitle,
  };
}
