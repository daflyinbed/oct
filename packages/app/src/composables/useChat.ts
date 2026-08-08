import { readonly, ref } from "vue";
import client from "@/api/client";
import type { components } from "@/api/schema";

type StoredMessage = components["schemas"]["StoredMessage"];
type AgentEvent = components["schemas"]["AgentEvent"];

export interface DisplayMessage {
  id: string;
  role: "user" | "assistant";
  content: string;
  isStreaming?: boolean;
}

const messages = ref<DisplayMessage[]>([]);
const sending = ref(false);

function storedToDisplay(m: StoredMessage): DisplayMessage {
  let content = "";
  try {
    const parts = JSON.parse(m.parts_json);
    content = parts
      .map((p: { Text?: string }) => {
        if (p.Text) return p.Text;
        return "";
      })
      .join("");
  } catch {
    content = m.parts_json;
  }
  return {
    id: m.id,
    role: m.role as "user" | "assistant",
    content,
  };
}

export function useChat() {
  const fetchMessages = async (conversationId: string) => {
    const { data, error } = await client.GET(
      "/api/conversations/{id}/messages",
      { params: { path: { id: conversationId } } },
    );
    if (!error && data) {
      messages.value = data.map(storedToDisplay);
    }
  };

  const sendMessage = async (
    conversationId: string,
    content: string,
    providerSpec?: string | null,
  ) => {
    if (!content.trim() || sending.value) return;
    sending.value = true;

    messages.value = [
      ...messages.value,
      { id: `pending-${Date.now()}`, role: "user", content },
    ];

    const assistantId = `stream-${Date.now()}`;
    messages.value = [
      ...messages.value,
      { id: assistantId, role: "assistant", content: "", isStreaming: true },
    ];

    try {
      const baseUrl = "http://127.0.0.1:3000";
      const body: { content: string; provider_spec?: string } = { content };
      if (providerSpec) body.provider_spec = providerSpec;
      const res = await fetch(
        `${baseUrl}/api/conversations/${conversationId}/messages`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(body),
        },
      );

      if (!res.ok || !res.body) {
        throw new Error(`HTTP ${res.status}`);
      }

      const reader = res.body.getReader();
      const decoder = new TextDecoder();
      let buffer = "";

      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        buffer += decoder.decode(value, { stream: true });

        const lines = buffer.split("\n");
        buffer = lines.pop() ?? "";

        for (const line of lines) {
          if (!line.startsWith("data: ")) continue;
          const payload = line.slice(6).trim();
          if (payload === "[DONE]") continue;

          try {
            const event: AgentEvent = JSON.parse(payload);
            if (event.type === "text_delta") {
              const idx = messages.value.findIndex((m) => m.id === assistantId);
              if (idx !== -1) {
                const updated = [...messages.value];
                updated[idx] = {
                  ...updated[idx],
                  content: updated[idx].content + event.data,
                };
                messages.value = updated;
              }
            } else if (event.type === "finish" || event.type === "cancelled") {
              const idx = messages.value.findIndex((m) => m.id === assistantId);
              if (idx !== -1) {
                const updated = [...messages.value];
                updated[idx] = { ...updated[idx], isStreaming: false };
                messages.value = updated;
              }
            } else if (event.type === "error") {
              const idx = messages.value.findIndex((m) => m.id === assistantId);
              if (idx !== -1) {
                const updated = [...messages.value];
                updated[idx] = {
                  ...updated[idx],
                  content: `${updated[idx].content}\n\nError: ${event.data}`,
                  isStreaming: false,
                };
                messages.value = updated;
              }
            }
          } catch {
            // skip malformed SSE
          }
        }
      }
    } catch {
      const idx = messages.value.findIndex((m) => m.id === assistantId);
      if (idx !== -1) {
        const updated = [...messages.value];
        updated[idx] = {
          ...updated[idx],
          content: "Failed to send message.",
          isStreaming: false,
        };
        messages.value = updated;
      }
    } finally {
      sending.value = false;
    }
  };

  const clearMessages = () => {
    messages.value = [];
  };

  return {
    messages: readonly(messages),
    sending: readonly(sending),
    fetchMessages,
    sendMessage,
    clearMessages,
  };
}
