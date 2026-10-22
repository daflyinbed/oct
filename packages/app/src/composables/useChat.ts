import {
  clearContextUsage,
  recordUsageEvent,
  seedContextUsage,
  setCompacting,
} from "@/composables/useContextUsage";
import { applySubagentEvent } from "@/composables/useSubagents";
import { createSession } from "./chat/session";
import type { SubagentUpdatedEventData } from "@/composables/useSubagents";

export { sendFailureText } from "./chat/api";
export * from "./chat/display";

/**
 * title_updated 事件监听器。标题是会话元数据而非消息内容：session 在
 * 有效流 owner 下按流所属会话派发，不进入消息 reducer。宿主 App.vue
 * 接线到 useProjects 的会话列表，由此更新侧栏标题。
 */
const titleListeners = new Set<
  (conversationId: string, title: string) => void
>();

/** 订阅 title_updated 事件；返回取消订阅函数。 */
export function onTitleUpdated(
  listener: (conversationId: string, title: string) => void,
): () => void {
  titleListeners.add(listener);
  return () => titleListeners.delete(listener);
}

export interface CreateChatSessionOptions {
  /**
   * 是否把 usage / 压缩状态写进全局 useContextUsage 单例（主面板输入框
   * 圆环的数据源）。子代理面板必须传 false——子 Finish/Usage 事件不得
   * 结束父流或覆盖父上下文环；实例自身的 usage 快照（返回值
   * 的 usage 字段）始终记录，供面板本地的用量环消费。默认 true。
   */
  contextUsage?: boolean;
  /**
   * `subagent_updated` 事件派发（SSE 消费层拦截，携带流所属会话 id 与
   * 负载）。仅主面板单例接线到 useSubagents 任务 store；子代理面板的
   * 实例不接线（子 hub 不发该事件，隔离由构造保证）。
   */
  onSubagentEvent?: (
    conversationId: string,
    data: SubagentUpdatedEventData,
  ) => void;
}

/** Create an isolated chat session; the main panel and child tabs share
 * implementation while keeping messages, subscriptions and usage separate. */
export function createChatSession(options: CreateChatSessionOptions = {}) {
  return createSession({
    usageSink:
      options.contextUsage === false
        ? undefined
        : {
            reset: clearContextUsage,
            seed: seedContextUsage,
            record: recordUsageEvent,
            setCompacting,
          },
    onSubagentEvent: options.onSubagentEvent,
    onTitleUpdated: (conversationId, title) => {
      // Listener exceptions are isolated and never mistaken for bad SSE frames.
      for (const listener of titleListeners) {
        try {
          listener(conversationId, title);
        } catch {
          /* isolate listeners */
        }
      }
    },
  });
}

/**
 * 主面板的模块级单会话实例：`subagent_updated` 事件在此接线到
 * useSubagents 任务 store（携带流所属会话 id，store 侧做迟到/跨会话
 * 隔离）。子代理面板一律经 createChatSession 另建实例，不碰单例。
 */
const singletonSession = createChatSession({
  onSubagentEvent: (conversationId, data) => {
    applySubagentEvent(conversationId, data);
  },
});

export function useChat() {
  return singletonSession;
}
