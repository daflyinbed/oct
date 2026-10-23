import { UNKNOWN_TOKEN_USAGE } from "@/composables/chat/tokenUsage";
import type { DisplayMessage, DisplayWork } from "@/composables/chat/display";
import type { TokenUsage } from "@/composables/chat/tokenUsage";

export interface ChatWork {
  kind: "work";
  key: string;
  history: DisplayMessage[];
  answer?: DisplayMessage;
  following: DisplayMessage[];
  state: DisplayWork["state"];
  defaultOpen: boolean;
  startedAt?: number;
  durationMs?: number;
}

export type ChatRenderUnit =
  { kind: "message"; key: string; message: DisplayMessage } | ChatWork;

/** 切分展示副本，原消息与服务器签发的切点保持不变。 */
function fragment(
  message: DisplayMessage,
  start: number,
  end: number,
): DisplayMessage {
  const isTail = end === message.parts.length;
  return {
    ...message,
    parts: message.parts.slice(start, end),
    isStreaming: isTail && message.isStreaming,
    forkCut: isTail ? message.forkCut : null,
    work: isTail ? message.work : undefined,
    finishedAt: isTail ? message.finishedAt : undefined,
  };
}

function splitWork(
  messages: DisplayMessage[],
  key: string,
  interrupted: boolean,
  turnUsage: readonly TokenUsage[],
  trigger?: DisplayMessage,
): ChatWork {
  const last = messages.at(-1)!;
  const work = last.work;
  const hasInterruptedCall = messages.some((message) =>
    message.parts.some(
      (part) => part.kind === "tool_call" && part.status === "interrupted",
    ),
  );
  const state =
    work?.state === "running" || messages.some((message) => message.isStreaming)
      ? "running"
      : work?.state === "failed" || work?.state === "interrupted"
        ? work.state
        : interrupted || hasInterruptedCall
          ? "interrupted"
          : "completed";

  // 优先使用服务器允许「从回复后分支」的正文锚点；旧历史与实时流没有
  // actions 元数据时，只把末尾的非空正文当作答案，工具前的说明不算。
  let answerMessage = -1;
  let answerPart = -1;
  if (state === "completed") {
    for (let i = messages.length - 1; i >= 0; i--) {
      const message = messages[i]!;
      if (
        message.forkCut?.kind !== "message" ||
        message.forkCut.message_id !== message.id ||
        message.forkCut.side !== "after"
      )
        continue;
      const index = message.parts.findLastIndex(
        (part) => part.kind === "text" && part.text.trim().length > 0,
      );
      if (index !== -1) {
        answerMessage = i;
        answerPart = index;
        break;
      }
    }
    if (answerMessage === -1) {
      const index = last.parts.findLastIndex(
        (part) => part.kind !== "text" || part.text.trim().length > 0,
      );
      if (index !== -1 && last.parts[index]?.kind === "text") {
        answerMessage = messages.length - 1;
        answerPart = index;
      }
    }
  }

  const history =
    answerMessage === -1 ? [...messages] : messages.slice(0, answerMessage);
  const following: DisplayMessage[] = [];
  let answer: DisplayMessage | undefined;
  if (answerMessage !== -1) {
    const message = messages[answerMessage]!;
    if (answerPart > 0) history.push(fragment(message, 0, answerPart));
    answer = fragment(message, answerPart, answerPart + 1);
    answer.turnUsage = turnUsage;
    if (answerPart + 1 < message.parts.length)
      following.push(fragment(message, answerPart + 1, message.parts.length));
    following.push(...messages.slice(answerMessage + 1));
  }

  // 消息 DTO 提供本轮开始与各次回复的结束时间；前端只用最终答复计算耗时。
  // 缺少时间戳的旧接口保留未知，取消与失败只展示状态。
  const start = trigger?.startedAt ?? messages[0]?.startedAt;
  const end =
    answerMessage === -1 ? undefined : messages[answerMessage]?.finishedAt;
  return {
    kind: "work",
    key,
    history,
    answer,
    following,
    state,
    defaultOpen: state !== "completed" || !answer,
    startedAt: start,
    durationMs:
      state === "running"
        ? 0
        : state === "completed" && start != null && end != null
          ? Math.max(0, end - start)
          : undefined,
  };
}

/** 实时单条 parts 与历史多条 assistant 记录共用同一分段规则。 */
export function buildChatRenderUnits(
  messages: readonly DisplayMessage[],
  options: { interrupted?: boolean } = {},
): ChatRenderUnit[] {
  const units: ChatRenderUnit[] = [];
  let assistants: DisplayMessage[] = [];
  let triggerId: string | undefined;
  let trigger: DisplayMessage | undefined;
  let turnUsage: TokenUsage[] = [];
  let segment = 0;
  const flush = (latest: boolean) => {
    if (assistants.length === 0) return;
    units.push(
      splitWork(
        assistants,
        `${triggerId ?? assistants[0]!.id}:work:${segment++}`,
        latest && Boolean(options.interrupted),
        turnUsage,
        trigger,
      ),
    );
    assistants = [];
  };
  for (const message of messages) {
    if (message.role !== "assistant") {
      flush(false);
      units.push({ kind: "message", key: message.id, message });
      if (message.role === "user" || message.role === "notification") {
        triggerId = message.id;
        trigger = message;
        turnUsage = [];
        segment = 0;
      }
      continue;
    }
    turnUsage.push(message.usage ?? UNKNOWN_TOKEN_USAGE);
    // 压缩分隔条始终可见，并保留原始顺序；不能被收入工作过程。
    let start = 0;
    for (const [index, part] of message.parts.entries()) {
      if (part.kind !== "compaction") continue;
      if (index > start) assistants.push(fragment(message, start, index));
      flush(false);
      const id = `${message.id}:compaction:${index}`;
      units.push({
        kind: "message",
        key: id,
        message: { id, role: "compaction", parts: [part] },
      });
      start = index + 1;
    }
    if (
      start < message.parts.length ||
      message.parts.length === 0 ||
      message.isStreaming
    ) {
      assistants.push(
        start === 0 ? message : fragment(message, start, message.parts.length),
      );
    }
  }
  flush(true);
  return units;
}

export function formatWorkDuration(ms: number): string {
  const seconds = Math.max(0, Math.floor(ms / 1000));
  const days = Math.floor(seconds / 86_400);
  const hours = Math.floor((seconds % 86_400) / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  const duration = `${minutes}分${seconds % 60}秒`;
  if (days > 0) return `${days}天${hours}小时${duration}`;
  if (hours > 0) return `${hours}小时${duration}`;
  return duration;
}
