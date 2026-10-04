import { readonly, ref } from "vue";
import client from "@/api/client";
import { useProjects } from "./useProjects";
import type { components } from "@/api/schema";

type Cut = components["schemas"]["ForkCut"];
type Request = components["schemas"]["RevertRequest"];

export function useConversationRevert() {
  const busy = ref(false);
  const error = ref<string | null>(null);
  const { upsertConversation } = useProjects();
  // An uncertain transport failure retries the same operation, even if the
  // server committed the backup and rewind before its response was lost.
  let pending: { source: string; body: Request } | null = null;

  async function revert(source: string, revision: number, cut: Cut) {
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
        "/api/conversations/{id}/revert",
        {
          params: { path: { id: source } },
          body: pending.body,
        },
      );
      if (failure || !data) {
        error.value =
          failure?.code === "stale_revision"
            ? "会话历史已变化，请刷新后重新选择回退位置。"
            : failure?.code === "already_running"
              ? "会话仍在运行，请等待结束后再回退。"
              : failure?.code === "background_pending"
                ? "请先完成或停止后台任务，并处理待投递通知或唤醒轮次，再回退。"
                : (failure?.error ?? "回退失败。");
        // A definite rejection has no committed operation to replay.
        pending = null;
        return null;
      }
      upsertConversation(data.conversation);
      upsertConversation(data.backup);
      pending = null;
      return data;
    } catch {
      error.value = "网络请求失败，可以重试回退。";
      return null;
    } finally {
      busy.value = false;
    }
  }
  return { busy: readonly(busy), error: readonly(error), revert };
}
