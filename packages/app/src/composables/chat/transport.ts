import type { ChatStreamEvent } from "./display";

/** Incremental SSE framing, including CRLF and multi-line data fields. The
 * boolean distinguishes a model terminal event from a transport disconnect. */
export async function readChatStream(
  response: Response,
  onEvent: (event: ChatStreamEvent) => void,
): Promise<boolean> {
  const reader = response.body!.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  let data: string[] = [];
  let terminal = false;
  const dispatch = () => {
    if (!data.length) return;
    const payload = data.join("\n");
    data = [];
    if (payload === "[DONE]") return;
    let event: ChatStreamEvent;
    try { event = JSON.parse(payload) as ChatStreamEvent; } catch { return; }
    if (["finish", "error", "cancelled"].includes(event.type)) terminal = true;
    onEvent(event);
  };
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      const lines = buffer.split("\n");
      buffer = lines.pop() ?? "";
      for (let line of lines) {
        if (line.endsWith("\r")) line = line.slice(0, -1);
        if (!line) dispatch();
        else if (line.startsWith("data:")) data.push(line.slice(5).replace(/^ /, ""));
      }
    }
    return terminal;
  } finally {
    reader.releaseLock();
  }
}
