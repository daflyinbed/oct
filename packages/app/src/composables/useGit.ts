import { onScopeDispose, ref, watch } from "vue";
import client from "@/api/client";
import { apiErrorMessage } from "@/api/errors";
import type { components } from "@/api/schema";
import type { Ref } from "vue";

export type GitFile = components["schemas"]["GitFile"];
export type GitStatus = components["schemas"]["GitStatus"];
export type GitEntry = components["schemas"]["GitEntry"];
export type GitAction = components["schemas"]["GitAction"];
export interface DiffSelection {
  path?: string;
  staged?: boolean;
  revision?: string;
  stash?: boolean;
}
export const gitChanged = ref(0);

export function useGit(projectId: Readonly<Ref<string | null>>) {
  const status = ref<GitStatus | null>(null);
  const history = ref<GitEntry[]>([]);
  const stashes = ref<GitEntry[]>([]);
  const error = ref("");
  const loading = ref(false);
  const busy = ref(false);
  let generation = 0;
  let controller: AbortController | undefined;

  async function refresh() {
    const id = projectId.value;
    const version = ++generation;
    controller?.abort();
    if (!id) {
      loading.value = false;
      return;
    }
    controller = new AbortController();
    loading.value = true;
    const options = {
      params: { path: { projectId: id } },
      signal: controller.signal,
    };
    try {
      const [state, commits, saved] = await Promise.all([
        client.GET("/api/projects/{projectId}/git/status", options),
        client.GET("/api/projects/{projectId}/git/history", options),
        client.GET("/api/projects/{projectId}/git/stashes", options),
      ]);
      if (version !== generation) return;
      if (!state.data) throw new Error(gitError(state.error));
      if (!commits.data) throw new Error(gitError(commits.error));
      if (!saved.data) throw new Error(gitError(saved.error));
      status.value = state.data;
      history.value = commits.data;
      stashes.value = saved.data;
      error.value = "";
    } catch (error_) {
      if (version === generation) error.value = gitError(error_);
    } finally {
      if (version === generation) loading.value = false;
    }
  }

  async function run(action: GitAction, paths?: string[], remote?: string) {
    const id = projectId.value;
    if (!id || busy.value) return;
    busy.value = true;
    error.value = "";
    let failure = "";
    try {
      const result = await client.POST(
        "/api/projects/{projectId}/git/actions",
        {
          params: { path: { projectId: id } },
          body: { action, paths, remote },
        },
      );
      if (!result.data) throw new Error(gitError(result.error));
    } catch (error_) {
      failure = gitError(error_);
    } finally {
      // A failed pull/stash-pop can still change files or leave conflicts.
      gitChanged.value++;
      if (id === projectId.value) {
        await refresh();
        if (failure) error.value = failure;
      }
      busy.value = false;
    }
  }

  watch(
    projectId,
    () => {
      status.value = null;
      history.value = [];
      stashes.value = [];
      error.value = "";
      void refresh();
    },
    { immediate: true },
  );
  const onFocus = () => {
    if (!busy.value) void refresh();
  };
  window.addEventListener("focus", onFocus);
  // Refresh external edits and agent tool writes while the panel is visible.
  const timer = window.setInterval(() => {
    if (
      document.visibilityState === "visible" &&
      !busy.value &&
      !loading.value &&
      !error.value
    )
      void refresh();
  }, 5000);
  onScopeDispose(() => {
    generation++;
    controller?.abort();
    window.clearInterval(timer);
    window.removeEventListener("focus", onFocus);
  });
  return { status, history, stashes, error, loading, busy, refresh, run };
}

export function gitError(cause: unknown): string {
  // Error 实例是本模块内部「已提取过业务文案」的再包装（refresh/run 的
  // throw 路径），透传其 message；其余形态走集中归一化。
  if (cause instanceof Error && cause.message) return cause.message;
  return apiErrorMessage(cause, "Git request failed");
}
