import client from "@/api/client";
import type { ApiMessage, ApprovalDecision } from "./display";
import type { SendBody } from "./submission";

export interface ChatTarget {
  kind: "conversation" | "subagent";
  id: string;
}
export interface HistorySnapshot {
  conversationId: string;
  messages: ApiMessage[];
  revision: number | null;
  historyEpoch: number | null;
  tokens: number | null;
}
const INTEGER_HEADER = /^\d+$/;
function integerHeader(response: Response | undefined, name: string) {
  const value = response?.headers.get(name);
  if (value == null || !INTEGER_HEADER.test(value)) return null;
  const parsed = Number(value);
  return Number.isSafeInteger(parsed) ? parsed : null;
}

async function history(target: ChatTarget): Promise<HistorySnapshot | null> {
  const result =
    target.kind === "conversation"
      ? await client.GET("/api/conversations/{id}/messages", {
          params: { path: { id: target.id } },
        })
      : await client.GET("/api/subagents/{agentId}/messages", {
          params: { path: { agentId: target.id } },
        });
  if (result.error || !result.data) return null;
  return {
    conversationId: target.id,
    messages: result.data,
    revision: integerHeader(result.response, "X-Context-Revision"),
    historyEpoch: integerHeader(result.response, "X-History-Epoch"),
    tokens: integerHeader(result.response, "X-Context-Tokens"),
  };
}
async function status(id: string, signal?: AbortSignal) {
  const { data } = await client.GET("/api/conversations/{id}/run", {
    params: { path: { id } },
    ...(signal ? { signal } : {}),
  });
  return data;
}
function events(target: ChatTarget, signal: AbortSignal) {
  return fetch(
    target.kind === "conversation"
      ? `/api/conversations/${target.id}/events`
      : `/api/subagents/${target.id}/events`,
    { signal },
  );
}
function send(id: string, body: SendBody, signal: AbortSignal) {
  return fetch(`/api/conversations/${id}/messages`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
    signal,
  });
}
function resume(id: string, signal: AbortSignal) {
  return fetch(`/api/conversations/${id}/resume`, { method: "POST", signal });
}
function approvals(id: string) {
  return client.GET("/api/conversations/{id}/approvals", {
    params: { path: { id } },
  });
}
function decideApproval(
  id: string,
  approvalId: string,
  decision: ApprovalDecision,
  pattern?: string,
) {
  return client.POST("/api/conversations/{id}/approvals/{approval_id}", {
    params: { path: { id, approval_id: approvalId } },
    body: { decision, pattern: pattern ?? null },
  });
}
function cancel(id: string) {
  return client.POST("/api/conversations/{id}/cancel", {
    params: { path: { id } },
  });
}
function compact(id: string) {
  return client.POST("/api/conversations/{id}/compact", {
    params: { path: { id } },
  });
}
function reload(id: string) {
  return client.POST("/api/conversations/{id}/instructions/reload", {
    params: { path: { id } },
  });
}
export const chatApi = {
  history,
  status,
  events,
  send,
  resume,
  approvals,
  decideApproval,
  cancel,
  compact,
  reload,
};

/** pending_recovery 引导文案（后端 409 code=pending_recovery）。 */
const PENDING_RECOVERY_HINT =
  "存在未完成的子代理任务，请使用「恢复此轮」继续或停止旧任务";

/**
 * 归一化 send 失败的可展示文本：错误体带 code=pending_recovery 时附引导
 * 文案（根会话有未结算的 crashed 子任务）；其余失败沿用既有提示（历史
 * 测试与 UI 均断言 "Failed to send message."）。非 JSON 错误体静默降级。
 */
export async function sendFailureText(res: Response): Promise<string> {
  try {
    const body = (await res.json()) as {
      code?: unknown;
      error?: unknown;
    };
    if (body?.code === "stale_revision")
      return "会话历史已变化，请刷新后重新发送。";
    if (body && typeof body === "object" && body.code === "pending_recovery") {
      const message =
        typeof body.error === "string" && body.error
          ? body.error
          : `HTTP ${res.status}`;
      return `Failed to send message.\n\n${message}\n${PENDING_RECOVERY_HINT}`;
    }
  } catch {
    // 非 JSON 错误体（如裸文本）：走通用文案
  }
  return "Failed to send message.";
}
