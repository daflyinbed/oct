import { readonly, ref, shallowReadonly } from "vue";
import client from "@/api/client";
import type { components } from "@/api/schema";

type Project = components["schemas"]["Project"];
type Conversation = components["schemas"]["Conversation"];

// ---------------------------------------------------------------------------
// 工具权限模式：三档词表与归一化（Conversation DTO 的
// permission_mode 是宽松 string，展示/提交前先收窄）。
// ---------------------------------------------------------------------------

export type PermissionMode = "default" | "accept_edits" | "bypass";

const PERMISSION_MODES: readonly PermissionMode[] = [
  "default",
  "accept_edits",
  "bypass",
];

/** 未知/缺失值归一为 default（旧数据或异常输入的兜底）。 */
export function normalizePermissionMode(
  value: string | null | undefined,
): PermissionMode {
  return (PERMISSION_MODES as readonly string[]).includes(value ?? "")
    ? (value as PermissionMode)
    : "default";
}

/** 模式切换器的选项（label 用于 AppSelect 触发器与菜单）。 */
export const PERMISSION_MODE_OPTIONS: readonly {
  value: PermissionMode;
  label: string;
}[] = [
  { value: "default", label: "默认" },
  { value: "accept_edits", label: "自动接受编辑" },
  { value: "bypass", label: "全部放行" },
];

/** 侧栏会话行的档位短标（default 不显示）。 */
export function permissionModeBadge(
  mode: string | null | undefined,
): { label: string; title: string } | null {
  const normalized = normalizePermissionMode(mode);
  if (normalized === "default") return null;
  return normalized === "accept_edits"
    ? { label: "自动编辑", title: "权限模式：工作区内的写入/编辑自动放行" }
    : { label: "全放行", title: "权限模式：全部放行（deny 规则仍然生效）" };
}

const projects = ref<Project[]>([]);
const conversations = ref<Map<string, Conversation[]>>(new Map());
const loading = ref(false);
const conversationGenerations = new Map<string, number>();

export function useProjects() {
  const upsertConversation = (conversation: Conversation) => {
    // A list request started before a history operation must not discard its
    // new backup or overwrite the original conversation's epoch/metadata.
    conversationGenerations.set(
      conversation.project_id,
      (conversationGenerations.get(conversation.project_id) ?? 0) + 1,
    );
    const existing = conversations.value.get(conversation.project_id) ?? [];
    conversations.value = new Map(conversations.value).set(
      conversation.project_id,
      [conversation, ...existing.filter((item) => item.id !== conversation.id)],
    );
  };
  const fetchConversations = async (projectId: string) => {
    const generation = (conversationGenerations.get(projectId) ?? 0) + 1;
    conversationGenerations.set(projectId, generation);
    const { data, error } = await client.GET(
      "/api/projects/{projectId}/conversations",
      { params: { path: { projectId } } },
    );
    if (
      !error &&
      data &&
      conversationGenerations.get(projectId) === generation
    ) {
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
        existing
          .filter((c) => c.id !== id)
          // Mirror source FKs' ON DELETE SET NULL in forks and backups.
          .map((c) => ({
            ...c,
            ...(c.forked_from_conversation_id === id
              ? { forked_from_conversation_id: null }
              : {}),
            ...(c.backup_of_conversation_id === id
              ? { backup_of_conversation_id: null }
              : {}),
          })),
      );
    }
  };

  /**
   * 本地更新某个会话的标题（title_updated 事件 / 手动改名落地时）。后端保证
   * 手动命名永不被自动标题覆盖，所以这里无条件覆盖即可。不命中任何列表则
   * 忽略——该会话尚未在侧栏加载，下次 fetchConversations 自然带上新标题。
   */
  const updateConversationTitle = (
    conversationId: string,
    title: string,
    source: Conversation["title_source"] = "ai",
  ) => {
    for (const [projectId, list] of conversations.value) {
      const index = list.findIndex((c) => c.id === conversationId);
      if (index !== -1) {
        const next = [...list];
        // title_source 一并补上：来源字段的价值在数据里，不留"default 配 AI
        // 标题"的矛盾状态（当前无读取方，但保持与后端落库结果一致）。
        next[index] = { ...next[index]!, title, title_source: source };
        conversations.value = new Map(conversations.value).set(projectId, next);
        return;
      }
    }
  };

  /**
   * 手动重命名会话（侧栏行内改名）。后端 PATCH 成功即写 title_source='user'，
   * 此后自动标题永不覆盖；响应无正文，成功后直接以请求值落地本地列表
   * （title_source 同步标 user，与落库结果一致）。空白标题不发请求直接
   * 返回 false——调用方按钮已按空值禁用，这里是双保险。
   */
  const renameConversation = async (
    projectId: string,
    conversationId: string,
    title: string,
  ) => {
    const trimmed = title.trim();
    if (!trimmed) return false;
    const { error } = await client.PATCH(
      "/api/projects/{projectId}/conversations/{id}",
      {
        params: { path: { projectId, id: conversationId } },
        body: { title: trimmed },
      },
    );
    if (error) return false;
    updateConversationTitle(conversationId, trimmed, "user");
    return true;
  };

  /**
   * 切换会话的工具权限模式：PATCH conversations 的
   * permission_mode，成功后同步本地列表（侧栏档位标即时更新）。逐次
   * 调用即时生效——活动 run 的下一笔工具调用就按新档走。会话不在任何
   * 已加载列表（找不到归属项目）时返回 false。
   */
  const setConversationPermissionMode = async (
    conversationId: string,
    mode: PermissionMode,
  ): Promise<boolean> => {
    for (const [projectId, list] of conversations.value) {
      const index = list.findIndex((c) => c.id === conversationId);
      if (index === -1) continue;
      const { error } = await client.PATCH(
        "/api/projects/{projectId}/conversations/{id}",
        {
          params: { path: { projectId, id: conversationId } },
          body: { permission_mode: mode },
        },
      );
      if (error) return false;
      const next = [...list];
      next[index] = { ...next[index]!, permission_mode: mode };
      conversations.value = new Map(conversations.value).set(projectId, next);
      return true;
    }
    return false;
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
    upsertConversation,
    deleteConversation,
    updateConversationTitle,
    renameConversation,
    setConversationPermissionMode,
  };
}
