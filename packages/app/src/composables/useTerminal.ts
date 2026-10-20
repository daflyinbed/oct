import { readonly, ref } from "vue";
import client from "@/api/client";
import type { components } from "@/api/schema";

export type TerminalInfo = components["schemas"]["TerminalInfo"];

export async function createTerminal(projectId: string): Promise<TerminalInfo> {
  const { data, error } = await client.POST(
    "/api/projects/{project_id}/terminals",
    {
      params: { path: { project_id: projectId } },
      body: { cols: 80, rows: 24 },
    },
  );
  if (error || !data) throw new Error(error?.error ?? "无法创建终端");
  return data;
}

export async function listTerminals(
  projectId: string,
): Promise<TerminalInfo[]> {
  const { data, error } = await client.GET(
    "/api/projects/{project_id}/terminals",
    {
      params: { path: { project_id: projectId } },
    },
  );
  if (error || !data) throw new Error(error?.error ?? "无法读取终端列表");
  return data;
}

export async function deleteTerminal(
  projectId: string,
  id: string,
): Promise<void> {
  const { error, response } = await client.DELETE(
    "/api/projects/{project_id}/terminals/{id}",
    {
      params: { path: { project_id: projectId, id } },
    },
  );
  if (error && response.status !== 404) throw new Error(error.error);
}

export type TerminalStatus =
  "connecting" | "connected" | "disconnected" | "exited" | "error";

interface TerminalSink {
  write: (bytes: Uint8Array) => void;
  reset: () => void;
  resize: (cols: number, rows: number) => void;
}

/** A view owns this connection; the server owns the shell. Disposing the
 * view never deletes a terminal, and reconnects never resubmit user input. */
export function createTerminalConnection(
  projectId: string,
  id: string,
  sink: TerminalSink,
) {
  const status = ref<TerminalStatus>("disconnected");
  const error = ref("");
  const notice = ref("");
  const exitCode = ref<number | null>(null);
  let socket: WebSocket | undefined;
  let offset = 0;
  let disposed = false;
  let retry: ReturnType<typeof setTimeout> | undefined;
  let handshake: ReturnType<typeof setTimeout> | undefined;
  let attempts = 0;
  let cols = 80;
  let rows = 24;
  let respondingToOutput = false;
  let replayReplyAfter = 0;

  function sendSize() {
    if (socket?.readyState === WebSocket.OPEN && status.value === "connected") {
      socket.send(JSON.stringify({ type: "resize", cols, rows }));
    }
  }

  function connect() {
    if (disposed || status.value === "exited") return;
    clearTimeout(retry);
    clearTimeout(handshake);
    const previous = socket;
    socket = undefined;
    previous?.close();
    status.value = "connecting";
    error.value = "";
    const url = new URL(
      `/api/projects/${encodeURIComponent(projectId)}/terminals/${encodeURIComponent(id)}/ws`,
      window.location.href,
    );
    url.protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
    url.searchParams.set("after", String(offset));
    const current = new WebSocket(url);
    socket = current;
    current.binaryType = "arraybuffer";
    handshake = setTimeout(() => {
      if (socket === current && status.value === "connecting") current.close();
    }, 15_000);
    current.addEventListener("message", (event: MessageEvent) => {
      if (socket !== current || disposed) return;
      try {
        if (event.data instanceof ArrayBuffer) {
          if (event.data.byteLength < 8) throw new Error("终端输出帧不完整");
          const start = Number(new DataView(event.data).getBigUint64(0));
          if (!Number.isSafeInteger(start) || start !== offset)
            throw new Error("终端输出顺序不一致，请重新连接");
          const bytes = new Uint8Array(event.data, 8);
          // A fresh view replays old queries to reconstruct terminal state,
          // but only queries beyond the server's processed watermark need a
          // reply. Split at that byte boundary even inside one output frame.
          const historical = Math.min(
            bytes.byteLength,
            Math.max(0, replayReplyAfter - start),
          );
          if (historical) sink.write(bytes.subarray(0, historical));
          if (historical < bytes.byteLength) {
            respondingToOutput = true;
            try {
              sink.write(bytes.subarray(historical));
            } finally {
              respondingToOutput = false;
            }
          }
          offset += bytes.byteLength;
          // Ghostty emits query replies synchronously from write(). They
          // precede this ack on the socket, so the server never marks a query
          // handled before receiving its reply.
          current.send(JSON.stringify({ type: "ack", offset }));
          return;
        }
        const message = JSON.parse(event.data as string);
        if (message.type === "ready") {
          if (
            !Number.isSafeInteger(message.replay_reply_after) ||
            message.replay_reply_after < 0
          )
            throw new Error("终端输出确认位置无效");
          replayReplyAfter = message.replay_reply_after;
          if (message.truncated) {
            sink.reset();
            notice.value = "较早的输出已超出缓存，已恢复最近的输出。";
          }
          offset = message.offset;
          // Replay at the server's last PTY dimensions before fitting the
          // live view; resizing first would reflow the history incorrectly.
          sink.resize(message.cols, message.rows);
        } else if (message.type === "live") {
          clearTimeout(handshake);
          status.value = "connected";
          attempts = 0;
          sendSize();
        } else if (message.type === "exit") {
          clearTimeout(handshake);
          status.value = "exited";
          exitCode.value = message.exit_code;
        } else if (message.type === "error") {
          throw new Error(message.message);
        }
      } catch (error_) {
        clearTimeout(handshake);
        error.value = error_ instanceof Error ? error_.message : "终端协议错误";
        status.value = "error";
        current.close();
      }
    });
    current.addEventListener("close", () => {
      if (socket !== current || disposed) return;
      clearTimeout(handshake);
      if (status.value === "exited" || status.value === "error") return;
      status.value = "disconnected";
      if (attempts < 5) {
        retry = setTimeout(connect, Math.min(500 * 2 ** attempts++, 8_000));
      } else {
        error.value =
          "连接失败。请重连；此终端可能已在其他页面打开，或后端已重启。";
      }
    });
    // Browsers intentionally hide handshake HTTP errors. onclose handles
    // bounded retries; server control errors above remain visible verbatim.
    current.addEventListener("error", () => {});
  }

  function write(data: string) {
    const startupReply = status.value === "connecting" && respondingToOutput;
    if (
      (status.value !== "connected" && !startupReply) ||
      socket?.readyState !== WebSocket.OPEN
    )
      return;
    const bytes = new TextEncoder().encode(data);
    if (socket.bufferedAmount + bytes.byteLength > 1024 * 1024) {
      error.value = "输入发送过快，请稍后再试。";
      return;
    }
    for (let start = 0; start < bytes.length; start += 16 * 1024) {
      socket.send(bytes.slice(start, start + 16 * 1024));
    }
  }

  function resize(nextCols: number, nextRows: number) {
    cols = Math.min(500, Math.max(2, nextCols));
    rows = Math.min(300, Math.max(1, nextRows));
    sendSize();
  }

  function dispose() {
    disposed = true;
    clearTimeout(retry);
    clearTimeout(handshake);
    const previous = socket;
    socket = undefined;
    previous?.close();
  }

  function reconnect() {
    attempts = 0;
    connect();
  }

  return {
    status: readonly(status),
    error: readonly(error),
    notice: readonly(notice),
    exitCode: readonly(exitCode),
    connect,
    reconnect,
    write,
    resize,
    dispose,
  };
}
