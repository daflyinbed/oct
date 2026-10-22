import { readonly, ref } from "vue";
import { lastAssistantInputTokens } from "./display";
import type { ApiMessage } from "./display";
import type { StreamEffect } from "./stream";

export interface UsageData {
  input_tokens?: number | null;
  context_window_tokens?: number | null;
}
export interface UsageSink {
  reset: () => void;
  seed: (messages: readonly ApiMessage[]) => void;
  record: (data: UsageData) => void;
  setCompacting: (value: boolean) => void;
}

/** Each panel owns its usage; only the compatibility facade injects a global sink. */
export function createChatUsage(sink?: UsageSink) {
  const inputTokens = ref<number | null>(null);
  const windowTokens = ref<number | null>(null);
  const compacting = ref(false);
  function record(data: UsageData) {
    if (data.input_tokens != null) inputTokens.value = data.input_tokens;
    if (data.context_window_tokens != null)
      windowTokens.value = data.context_window_tokens;
    sink?.record(data);
  }
  function setCompacting(value: boolean) {
    compacting.value = value;
    sink?.setCompacting(value);
  }
  function seed(
    messages: readonly ApiMessage[],
    tokens: number | null,
    child: boolean,
  ) {
    inputTokens.value = tokens ?? lastAssistantInputTokens(messages);
    if (child) windowTokens.value = null;
    sink?.seed(messages);
    if (tokens != null) sink?.record({ input_tokens: tokens });
  }
  function reset() {
    inputTokens.value = null;
    windowTokens.value = null;
    compacting.value = false;
    sink?.reset();
  }
  function apply(effect: StreamEffect) {
    if (effect.kind === "usage") record(effect.data);
    if (effect.kind === "compacting") setCompacting(effect.value);
  }
  return {
    state: {
      inputTokens: readonly(inputTokens),
      windowTokens: readonly(windowTokens),
      compacting: readonly(compacting),
    },
    record,
    setCompacting,
    seed,
    reset,
    apply,
  };
}

export type ChatUsage = ReturnType<typeof createChatUsage>;
