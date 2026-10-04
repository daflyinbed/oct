import { readonly, ref } from "vue";
import client from "@/api/client";
import { useProjects } from "./useProjects";
import type { components } from "@/api/schema";

type ForkCut = components["schemas"]["ForkCut"];
type ForkRequest = components["schemas"]["ForkRequest"];

export function useConversationFork() {
  const busy = ref(false);
  const error = ref<string | null>(null);
  const { upsertConversation } = useProjects();
  // Retain the nonce after an uncertain transport failure. A retry must not
  // create another conversation when the original response was lost.
  let pending: { source: string; body: ForkRequest } | null = null;

  async function fork(source: string, revision: number, cut: ForkCut) {
    if (busy.value) return null;
    if (
      !pending ||
      pending.source !== source ||
      pending.body.expected_revision !== revision ||
      JSON.stringify(pending.body.cut) !== JSON.stringify(cut)
    ) {
      pending = {
        source,
        body: {
          operation_id: crypto.randomUUID(),
          expected_revision: revision,
          cut,
        },
      };
    }
    busy.value = true;
    error.value = null;
    try {
      const { data, error: failure } = await client.POST(
        "/api/conversations/{id}/fork",
        {
          params: { path: { id: source } },
          body: pending.body,
        },
      );
      if (failure || !data) {
        error.value =
          failure?.code === "stale_revision"
            ? "会话历史已变化，请刷新后重新选择分支位置。"
            : failure?.code === "already_running"
              ? "会话仍在运行，请等待完成后再创建分支。"
              : (failure?.error ?? "创建分支失败。");
        return null;
      }
      upsertConversation(data.conversation);
      pending = null;
      return data;
    } catch {
      error.value = "网络请求失败，可以重试创建分支。";
      return null;
    } finally {
      busy.value = false;
    }
  }
  return { busy: readonly(busy), error: readonly(error), fork };
}
