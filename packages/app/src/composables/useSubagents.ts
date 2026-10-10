import { computed, readonly, ref, toValue } from "vue";
import client from "@/api/client";
import { useTabs } from "@/composables/useTabs";
import type { components } from "@/api/schema";
import type { MaybeRefOrGetter } from "vue";

// ---------------------------------------------------------------------------
// 类型：快照 DTO 与事件负载直接取自生成的 OpenAPI schema；本文件只在
// store 形状上补 `activity`（事件带来的实时摘要，快照端点不返回）并把
// `status` 收窄为字面量联合。
// ---------------------------------------------------------------------------

export type SubagentStatus =
  | "running"
  | "completed"
  | "failed"
  | "cancelled"
  | "lost";

const SUBAGENT_STATUSES: readonly SubagentStatus[] = [
  "running",
  "completed",
  "failed",
  "cancelled",
  "lost",
];

export function isSubagentStatus(value: unknown): value is SubagentStatus {
  return (
    typeof value === "string" &&
    (SUBAGENT_STATUSES as readonly string[]).includes(value)
  );
}

/**
 * 父 hub `subagent_updated` 事件的 data 负载：会话作用域的小状态事件，
 * 供 TaskCard 与底部指示器展示（完整 text/tool 事件留在子 hub，打开
 * panel 时按需订阅）。
 */
export type SubagentUpdatedEventData = Extract<
  components["schemas"]["AgentEvent"],
  { type: "subagent_updated" }
>["data"];

/** GET /api/conversations/{id}/subagents 的任务快照行。 */
export type SubagentTaskDto = components["schemas"]["SubagentTaskDto"];

/** store 内形状：快照 DTO + status 收窄 + 事件带来的实时活动摘要。 */
export type SubagentTask = Omit<SubagentTaskDto, "status"> & {
  status: SubagentStatus;
  activity: string | null;
};

// ---------------------------------------------------------------------------
// 根会话任务 store（模块级单例，useMcpStatus 同款模式）：独立于当前可见
// panel——切到子标签或关闭底部弹层后，仍通过根 run 的 subagent_updated
// 事件更新任务列表，不需要常驻订阅所有子任务的完整输出。
// ---------------------------------------------------------------------------

const tasks = ref<SubagentTask[]>([]);
const loadedConversationId = ref<string | null>(null);

/**
 * 请求序号：refresh 与会话切换可能并发（连点会话/开菜单），只有最新一次
 * 调用的响应允许写入状态，旧根会话的迟到响应不能污染新根会话。
 */
let refreshSeq = 0;
let eventVersion = 0;
const taskEventVersions = new Map<string, number>();

/**
 * 拉取 conversationId 的子任务快照（running 优先、created_at 升序）。
 * conversationId 为 null 时清空（离开会话/空态）。失败保留旧列表静默返回。
 */
export async function refreshSubagents(
  conversationId: string | null,
): Promise<void> {
  if (!conversationId) {
    refreshSeq += 1; // 使在途旧响应失效
    tasks.value = [];
    loadedConversationId.value = null;
    taskEventVersions.clear();
    return;
  }
  const seq = ++refreshSeq;
  const version = eventVersion;
  const { data, error } = await client.GET(
    "/api/conversations/{id}/subagents",
    { params: { path: { id: conversationId } } },
  );
  if (!error && data != null && seq === refreshSeq) {
    if (loadedConversationId.value !== conversationId)
      taskEventVersions.clear();
    const live = new Map(
      loadedConversationId.value === conversationId
        ? tasks.value.map((task) => [task.run_id, task] as const)
        : [],
    );
    const snapshot = data.map(
      (task): SubagentTask => ({
        ...task,
        // 生成的 DTO 里 status 是 string；按已知词表收窄（未知值按运行中
        // 处理，与事件路径一致——宁可多显示活动，不漏 lost 待恢复）。
        status: isSubagentStatus(task.status) ? task.status : "running",
        activity: null,
      }),
    );
    const seen = new Set(snapshot.map((task) => task.run_id));
    tasks.value = snapshot.map((task) => {
      const current = live.get(task.run_id);
      if (
        current &&
        (current.execution_epoch > task.execution_epoch ||
          (current.execution_epoch === task.execution_epoch &&
            ((taskEventVersions.get(task.run_id) ?? 0) > version ||
              (current.status !== "running" && task.status === "running"))))
      )
        return current;
      return task;
    });
    // Tasks created by events after the GET snapshot must not disappear.
    for (const current of live.values()) {
      if (
        !seen.has(current.run_id) &&
        (taskEventVersions.get(current.run_id) ?? 0) > version
      ) {
        tasks.value.push(current);
      }
    }
    loadedConversationId.value = conversationId;
  }
}

/**
 * `subagent_updated` 事件落地：按 run_id upsert。execution_epoch 更小的
 * 迟到事件（crash 恢复前的旧执行实例）直接忽略；新任务只吸收已加载
 * 会话自己的事件——别的会话的迟到事件不得混入当前列表。
 */
export function applySubagentEvent(
  conversationId: string,
  data: SubagentUpdatedEventData,
): void {
  if (loadedConversationId.value !== conversationId) return;
  const index = tasks.value.findIndex((task) => task.run_id === data.run_id);
  if (index === -1) {
    taskEventVersions.set(data.run_id, ++eventVersion);
    tasks.value = [
      ...tasks.value,
      {
        run_id: data.run_id,
        agent_id: data.agent_id,
        description: data.description,
        status: data.status,
        execution_epoch: data.execution_epoch,
        parent_call_id: data.parent_call_id,
        provider_id: null,
        model_id: null,
        input_tokens: null,
        output_tokens: null,
        // 事件不带时间戳：新插入行的创建时间以到达时刻近似，快照刷新会对齐。
        created_at: new Date().toISOString(),
        finished_at: null,
        final_message_id: null,
        title: data.description,
        running: data.status === "running",
        activity: data.activity ?? null,
      },
    ];
    return;
  }
  const existing = tasks.value[index]!;
  if (data.execution_epoch < existing.execution_epoch) return;
  if (
    data.execution_epoch === existing.execution_epoch &&
    existing.status !== "running" &&
    data.status === "running"
  )
    return;
  taskEventVersions.set(data.run_id, ++eventVersion);
  tasks.value = tasks.value.map((task, i) =>
    i === index
      ? {
          ...task,
          status: data.status,
          execution_epoch: data.execution_epoch,
          description: data.description,
          running: data.status === "running",
          activity: data.activity ?? null,
          finished_at:
            data.status === "running"
              ? null
              : (task.finished_at ?? new Date().toISOString()),
        }
      : task,
  );
}

/** 按父调用的 call id 查任务（TaskCard 运行中拿 agent_id 的数据源）。 */
export function taskByCallId(parentCallId: string): SubagentTask | null {
  return (
    tasks.value.find((task) => task.parent_call_id === parentCallId) ?? null
  );
}

/** 按 agent（子会话）id 查任务（子 panel 头部的数据源）。 */
export function taskByAgentId(agentId: string): SubagentTask | null {
  return tasks.value.find((task) => task.agent_id === agentId) ?? null;
}

/**
 * 打开（或激活已有的）子代理标签页。标签按 `subagent:{agent_id}` 去重：
 * 重复点击只激活，相同标题的不同子代理各有标签。rootConversationId
 * 供 panel 的「返回主对话」跳回所属根会话。
 */
export function openSubagentPanel(
  agentId: string,
  description: string,
  rootConversationId?: string | null,
): void {
  const { openTab } = useTabs();
  openTab({
    id: `subagent:${agentId}`,
    kind: "subagent",
    payload: agentId,
    title: `子代理 · ${description}`,
    rootConversationId: rootConversationId ?? null,
  });
}

/**
 * 请求取消单个子任务（POST；404 = 已结束，忽略即可）。UI 状态由
 * subagent_updated 事件（cancelled）落定而非本响应。
 */
export async function cancelSubagent(agentId: string): Promise<void> {
  await client.POST("/api/subagents/{agentId}/cancel", {
    params: { path: { agentId } },
  });
}

const runningCount = computed(
  () => tasks.value.filter((task) => task.status === "running").length,
);
const hasLost = computed(() =>
  tasks.value.some((task) => task.status === "lost"),
);

export function useSubagents() {
  /** 当前会话的任务列表：running 优先，其余按 created_at 升序（与快照一致）。 */
  function subagentsFor(conversationId: MaybeRefOrGetter<string | null>) {
    return computed(() => {
      const id = toValue(conversationId);
      if (id == null || loadedConversationId.value !== id) return [];
      return sortSubagentTasks(tasks.value);
    });
  }

  return {
    tasks: readonly(tasks),
    loadedConversationId: readonly(loadedConversationId),
    refreshSubagents,
    applySubagentEvent,
    subagentsFor,
    taskByCallId,
    taskByAgentId,
    openSubagentPanel,
    cancelSubagent,
    runningCount,
    hasLost,
  };
}

/** running 优先，其余按 created_at 升序；Array#sort 稳定，快照原序保持。 */
function sortSubagentTasks(list: readonly SubagentTask[]): SubagentTask[] {
  return list.toSorted((a, b) => {
    const aRunning = a.status === "running";
    const bRunning = b.status === "running";
    if (aRunning !== bRunning) return aRunning ? -1 : 1;
    return a.created_at.localeCompare(b.created_at);
  });
}

// ---------------------------------------------------------------------------
// 状态展示（TaskCard 与底部指示器共用）
// ---------------------------------------------------------------------------

export function subagentStatusDot(status: SubagentStatus): string {
  return (
    {
      running: "bg-accent-7",
      completed: "bg-success-10",
      failed: "bg-danger-10",
      cancelled: "bg-neutral-5",
      lost: "bg-warning-10",
    }[status] ?? "bg-neutral-5"
  );
}

export function subagentStatusLabel(status: SubagentStatus): string {
  return (
    {
      running: "运行中",
      completed: "已完成",
      failed: "失败",
      cancelled: "已取消",
      lost: "待恢复",
    }[status] ?? status
  );
}

export function subagentStatusClass(status: SubagentStatus): string {
  return (
    {
      running: "text-accent-10",
      completed: "text-success-10",
      failed: "text-danger-10",
      cancelled: "text-faint",
      lost: "text-warning-10",
    }[status] ?? "text-faint"
  );
}

/**
 * 任务耗时短格式：运行中 endIso 传 null（以 now 计），终态传 finished_at。
 * 无法解析/倒挂的时间显示 "—"。
 */
export function formatSubagentElapsed(
  startIso: string | null,
  endIso: string | null,
  now: number = Date.now(),
): string {
  if (!startIso) return "—";
  const start = Date.parse(startIso);
  const end = endIso != null ? Date.parse(endIso) : now;
  if (!Number.isFinite(start) || !Number.isFinite(end) || end < start) {
    return "—";
  }
  return formatDurationMs(end - start);
}

/** 8300 → "8s"；83s → "1m 23s"；3700s → "1h 1m"。 */
export function formatDurationMs(ms: number): string {
  if (ms < 0) return "—";
  if (ms < 1000) return "0s";
  const totalSeconds = Math.floor(ms / 1000);
  if (totalSeconds < 60) return `${totalSeconds}s`;
  const minutes = Math.floor(totalSeconds / 60);
  if (minutes < 60) return `${minutes}m ${totalSeconds % 60}s`;
  const hours = Math.floor(minutes / 60);
  return `${hours}h ${minutes % 60}m`;
}
