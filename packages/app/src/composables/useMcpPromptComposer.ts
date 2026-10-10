import { onScopeDispose, readonly, ref, toValue, watch } from "vue";
import { renderMcpPrompt } from "./useMcpServers";
import { formatPromptMessages, replaceSlashCommand } from "./useSlashCommands";
import type { MaybeRefOrGetter, Ref } from "vue";

/** Owns asynchronous prompt rendering for one composer. Editing its input,
 * switching conversations, or starting another render invalidates old replies. */
export function useMcpPromptComposer(
  conversationId: MaybeRefOrGetter<string | null>,
  input: Ref<string>,
) {
  const rendering = ref(false);
  const error = ref<string | null>(null);
  let generation = 0;

  function invalidate() {
    ++generation;
    rendering.value = false;
    error.value = null;
  }
  watch([() => toValue(conversationId), input], invalidate, { flush: "sync" });
  onScopeDispose(invalidate);

  async function render(
    serverId: string,
    promptName: string,
    arguments_: Record<string, string>,
  ): Promise<string | null> {
    const request = ++generation;
    rendering.value = true;
    error.value = null;
    try {
      const { messages } = await renderMcpPrompt(
        serverId,
        promptName,
        arguments_,
      );
      if (request !== generation) return null;
      const text = formatPromptMessages(messages);
      input.value = replaceSlashCommand(input.value, text);
      return text;
    } catch (error_) {
      if (request === generation)
        error.value = error_ instanceof Error ? error_.message : String(error_);
      return null;
    } finally {
      if (request === generation) rendering.value = false;
    }
  }

  return { rendering: readonly(rendering), error: readonly(error), render };
}
