import type { ChatStreamEvent } from "./display";

const LEADING_SPACE = /^ /;

/** Incremental SSE framing, including CRLF and multi-line data fields. The
 * boolean distinguishes a model terminal event from a transport disconnect. */
export async function readChatStream(
  response: Response,
  onEvent: (event: ChatStreamEvent) => unknown | Promise<unknown>,
  signal?: AbortSignal,
): Promise<boolean> {
  const reader = response.body!.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  let data: string[] = [];
  let terminal = false;
  let reachedEof = false;
  const abort = () => {
    void reader.cancel().catch(() => {});
  };
  signal?.addEventListener("abort", abort, { once: true });
  const dispatch = async () => {
    if (data.length === 0) return;
    const payload = data.join("\n");
    data = [];
    if (payload === "[DONE]") return;
    let event: ChatStreamEvent;
    try {
      event = JSON.parse(payload) as ChatStreamEvent;
    } catch {
      return;
    }
    if (["finish", "error", "cancelled"].includes(event.type)) terminal = true;
    if (!signal?.aborted) await onEvent(event);
  };
  try {
    while (!signal?.aborted) {
      const { done, value } = await reader.read();
      if (done) {
        reachedEof = true;
        break;
      }
      buffer += decoder.decode(value, { stream: true });
      const lines = buffer.split("\n");
      buffer = lines.pop() ?? "";
      for (let line of lines) {
        if (line.endsWith("\r")) line = line.slice(0, -1);
        if (!line) await dispatch();
        else if (line.startsWith("data:"))
          data.push(line.slice(5).replace(LEADING_SPACE, ""));
      }
    }
    return terminal;
  } finally {
    signal?.removeEventListener("abort", abort);
    if (signal?.aborted || !reachedEof) await reader.cancel().catch(() => {});
    reader.releaseLock();
  }
}
