import { readonly, ref } from "vue";
import client from "@/api/client";
import type { components } from "@/api/schema";

type Project = components["schemas"]["Project"];
type Conversation = components["schemas"]["Conversation"];

const projects = ref<Project[]>([]);
const conversations = ref<Map<string, Conversation[]>>(new Map());
const loading = ref(false);

export function useProjects() {
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

  const fetchConversations = async (projectId: string) => {
    const { data, error } = await client.GET(
      "/api/projects/{projectId}/conversations",
      { params: { path: { projectId } } },
    );
    if (!error && data) {
      conversations.value = new Map(conversations.value).set(projectId, data);
    }
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

  return {
    projects: readonly(projects),
    conversations: readonly(conversations),
    loading: readonly(loading),
    fetchProjects,
    fetchConversations,
    createProject,
    createConversation,
    deleteConversation,
  };
}
