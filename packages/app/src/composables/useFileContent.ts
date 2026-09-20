import { readonly, ref } from "vue";
import client from "@/api/client";
import type { components } from "@/api/schema";

export type FileContent = components["schemas"]["FileContent"];

/** 拉取项目工作目录内单个文本文件的内容（预览用，后端在 1 MiB 处截断）。 */
export function useFileContent(projectId: string | null, path: string) {
  const data = ref<FileContent | null>(null);
  const loading = ref(false);
  const error = ref<string | null>(null);

  async function load(id: string) {
    loading.value = true;
    error.value = null;
    const { data: result, error: err } = await client.GET(
      "/api/projects/{projectId}/files/content",
      { params: { path: { projectId: id }, query: { path } } },
    );
    if (err || !result) {
      error.value =
        (err as unknown as { error?: string } | undefined)?.error ??
        "Failed to load file";
    } else {
      data.value = result;
    }
    loading.value = false;
  }

  if (projectId) {
    void load(projectId);
  } else {
    error.value = "No project is active for this file.";
  }

  return {
    data: readonly(data),
    loading: readonly(loading),
    error: readonly(error),
  };
}
