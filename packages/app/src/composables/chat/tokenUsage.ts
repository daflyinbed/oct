import type { components } from "@/api/schema";

export interface TokenUsage {
  providerId: string | null;
  modelId: string | null;
  inputTokens: number | null;
  outputTokens: number | null;
  reasoningTokens: number | null;
  cachedInputTokens: number | null;
  cacheCreationTokens: number | null;
}

export const UNKNOWN_TOKEN_USAGE: TokenUsage = Object.freeze({
  providerId: null,
  modelId: null,
  inputTokens: null,
  outputTokens: null,
  reasoningTokens: null,
  cachedInputTokens: null,
  cacheCreationTokens: null,
});

function tokenCount(value: unknown): number | null {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0
    ? value
    : null;
}

export function messageTokenUsage(
  message: components["schemas"]["MessageDto"],
  providerId: string | null,
  modelId: string | null,
): TokenUsage {
  const value = message.details?.extra?.usage;
  const snapshot =
    value && typeof value === "object" && !Array.isArray(value)
      ? (value as Record<string, unknown>)
      : {};
  const inputTokens =
    tokenCount(snapshot.input_tokens) ?? tokenCount(message.input_tokens);
  const outputTokens =
    tokenCount(snapshot.output_tokens) ?? tokenCount(message.output_tokens);
  return {
    providerId: message.provider_id ?? providerId,
    modelId: message.model_id ?? modelId,
    inputTokens,
    outputTokens,
    reasoningTokens:
      tokenCount(snapshot.reasoning_tokens) ??
      tokenCount(message.reasoning_tokens),
    cachedInputTokens: tokenCount(snapshot.cached_input_tokens),
    cacheCreationTokens: tokenCount(snapshot.cache_creation_input_tokens),
  };
}

/** Missing provider measurements stay unknown rather than becoming zero. */
export function sumTokens(
  usage: readonly TokenUsage[],
  field: Exclude<keyof TokenUsage, "providerId" | "modelId">,
): number | null {
  if (usage.length === 0 || usage.some((item) => item[field] == null))
    return null;
  return usage.reduce((sum, item) => sum + (item[field] as number), 0);
}
