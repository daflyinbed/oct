import { ref } from "vue";
import client from "@/api/client";
import type { ToolCallPart } from "@/composables/chat/display";

/** Keep the fetched original for the card's lifetime, including folds. */
export function useToolOriginal(
  part: () => ToolCallPart,
  conversationId: () => string | null,
) {
  // --- 微压缩「查看原文」 ------------------------------------------------------

  const originalText = ref<string | null>(null);
  const loadingOriginal = ref(false);
  const originalFailed = ref(false);

  /** 经副本 id 回查被 supersedes 的原文（一次加载，失败可重试）。 */
  async function loadOriginal(): Promise<void> {
    const copyId = part().clearedCopyMessageId;
    const id = conversationId();
    if (
      !copyId ||
      !id ||
      loadingOriginal.value ||
      originalText.value !== null
    ) {
      return;
    }
    loadingOriginal.value = true;
    originalFailed.value = false;
    try {
      const { data, error } = await client.GET(
        "/api/conversations/{id}/messages/{copy_id}/original",
        {
          params: {
            path: { id, copy_id: copyId },
          },
        },
      );
      if (error || !data) {
        originalFailed.value = true;
        return;
      }
      // 类型化 parts（与历史端点同一 DTO）；非 tool_result 的部分跳过。
      const chunks: string[] = [];
      for (const part of data.parts) {
        if (part.type !== "tool_result") continue;
        chunks.push(
          typeof part.content === "string"
            ? part.content
            : JSON.stringify(part.content ?? "", null, 2),
        );
      }
      originalText.value = chunks.join("\n");
    } catch {
      // 网络层异常（fetch 拒绝）：与 HTTP 错误同路径，允许重试。
      originalFailed.value = true;
    } finally {
      loadingOriginal.value = false;
    }
  }

  return { originalText, loadingOriginal, originalFailed, loadOriginal };
}
