import { computed, readonly, ref, toValue } from "vue";
import client from "@/api/client";
import type { components } from "@/api/schema";
import type { MaybeRefOrGetter } from "vue";

// ---------------------------------------------------------------------------
// 类型：快照 DTO 直接取自生成的 OpenAPI schema；本文件只在 store 形状上把
// `status` 收窄为字面量联合（useSubagents 同款模式）。
// ---------------------------------------------------------------------------

export type BackgroundTaskStatus =
  "running" | "completed" | "failed" | "cancelled" | "lost";

const BACKGROUND_TASK_STATUSES: readonly BackgroundTaskStatus[] = [
  "running",
  "completed",
  "failed",
  "cancelled",
  "lost",
];

export function isBackgroundTaskStatus(
  value: unknown,
): value is BackgroundTaskStatus {
  return (
    typeof value === "string" &&
    (BACKGROUND_TASK_STATUSES as readonly string[]).includes(value)
  );
}

/** GET /api/conversations/{id}/background-tasks 的任务行。 */
export type BackgroundTaskDto = components["schemas"]["BackgroundTaskDto"];

/** store 内形状：快照 DTO + status 收窄。 */
export type BackgroundTask = Omit<BackgroundTaskDto, "status"> & {
  status: BackgroundTaskStatus;
};

/** 终态判定：running 之外都是（lost = 后端重启击杀，等待通知补投）。 */
export function isBackgroundTaskTerminal(
  status: BackgroundTaskStatus,
): boolean {
  return status !== "running";
}

// ---------------------------------------------------------------------------
// 会话任务 store（模块级单例，useSubagents 同款模式）：按当前会话保存任务
// 快照。PR1 刻意没有 hub 小事件（PR3 再做），所以状态靠轮询对账——存在
// 非终态任务时每 2s 刷新一次，全部结算后多守一个「唤醒宽限窗」探测
// /run（后端在任务终态后会自动起 wake run，前端要在无 SSE 订阅的情况
// 下发现并挂上它的流）。
// ---------------------------------------------------------------------------

const POLL_INTERVAL_MS = 2000;
/** 全部结算后继续探测 /run 的宽限窗（等后端投递循环起 wake run）。 */
export const WAKE_GRACE_MS = 30_000;

const tasks = ref<BackgroundTask[]>([]);
const loadedConversationId = ref<string | null>(null);

/**
 * 请求序号：refresh 与会话切换可能并发，只有最新一次调用的响应允许写入
 * 状态（照抄 useSubagents 的 refreshSeq 模式）。
 */
let refreshSeq = 0;

/** 轮询目标会话（null = 停止）。单链 setTimeout 循环，tick 内自续。 */
let pollConversationId: string | null = null;
let pollTimer: ReturnType<typeof setTimeout> | null = null;
/** 唤醒宽限窗截止（epoch ms）：最后一次观测到任务结算/run 活跃的时间 +30s。 */
let wakeGraceUntil = 0;
/** /run 探测与 wake 挂接的串行化（一个在飞的探测就够）。 */
let probingRun = false;
/** wake run 挂接器（P1）：App.vue 接线到 useChat 的 attachRun。 */
let wakeRunHandler: ((conversationId: string) => unknown) | null = null;

function narrowStatus(raw: string): BackgroundTaskStatus {
  // 未知值按运行中处理（宁可多显示活动，与 useSubagents 一致）。
  return isBackgroundTaskStatus(raw) ? raw : "running";
}

function hasActiveTask(list: readonly BackgroundTask[]): boolean {
  return list.some((task) => !isBackgroundTaskTerminal(task.status));
}

/**
 * 提交新快照并检测「本 tick 内 running → 终态」的结算转换（refresh 整表
 * 替换与 cancel 单行 upsert 共用）：一旦观测到转换就开/顺延唤醒宽限窗
 * ——任务结算后后端会投递通知并起 wake run，前端要在这窗口内探测 /run。
 * 当前会话中新出现的终态行也开窗（快速任务可能来不及观测到 running）；
 * 首次加载另一会话的历史终态行不开窗。
 */
function commitSnapshot(next: BackgroundTask[]): void {
  const prevById = new Map(tasks.value.map((task) => [task.id, task]));
  let settledNow = false;
  for (const row of next) {
    const prev = prevById.get(row.id);
    if (
      (prev
        ? !isBackgroundTaskTerminal(prev.status)
        : loadedConversationId.value === row.conversation_id) &&
      isBackgroundTaskTerminal(row.status)
    ) {
      settledNow = true;
    }
  }
  tasks.value = next;
  if (settledNow) wakeGraceUntil = Date.now() + WAKE_GRACE_MS;
}

/** running 优先，其余按 created_at 升序（与快照端点的排序契约一致）。 */
function sortBackgroundTasks(
  list: readonly BackgroundTask[],
): BackgroundTask[] {
  return list.toSorted((a, b) => {
    const aRunning = !isBackgroundTaskTerminal(a.status);
    const bRunning = !isBackgroundTaskTerminal(b.status);
    if (aRunning !== bRunning) return aRunning ? -1 : 1;
    return a.created_at.localeCompare(b.created_at);
  });
}

/** 按行 upsert（cancel 响应落位用）；别的会话的行不混入当前列表。 */
function upsertTask(row: BackgroundTaskDto): void {
  if (loadedConversationId.value !== row.conversation_id) return;
  const narrowed: BackgroundTask = { ...row, status: narrowStatus(row.status) };
  const rest = tasks.value.filter((task) => task.id !== narrowed.id);
  commitSnapshot(sortBackgroundTasks([narrowed, ...rest]));
}

/**
 * 拉取 conversationId 的任务快照（running 优先、最旧在前）。
 * conversationId 为 null 时清空（离开会话/空态）并停轮询。失败保留旧列表
 * 静默返回。快照里存在非终态任务时自动启动轮询。
 */
export async function refreshBackgroundTasks(
  conversationId: string | null,
): Promise<void> {
  if (!conversationId) {
    refreshSeq += 1; // 使在途旧响应失效
    tasks.value = [];
    loadedConversationId.value = null;
    wakeGraceUntil = 0; // 旧会话的唤醒守望不带入新会话
    stopPolling();
    return;
  }
  const seq = ++refreshSeq;
  const { data, error } = await client.GET(
    "/api/conversations/{id}/background-tasks",
    { params: { path: { id: conversationId } } },
  );
  if (!error && data != null && seq === refreshSeq) {
    if (loadedConversationId.value !== conversationId) {
      wakeGraceUntil = 0;
      stopPolling();
    }
    commitSnapshot(
      sortBackgroundTasks(
        data.map((row) => ({ ...row, status: narrowStatus(row.status) })),
      ),
    );
    loadedConversationId.value = conversationId;
    if (hasActiveTask(tasks.value) || Date.now() < wakeGraceUntil)
      startPolling(conversationId);
  }
}

/** 按任务 id 查快照（信封卡片的 live 数据源）。 */
export function backgroundTaskById(taskId: string): BackgroundTask | null {
  return tasks.value.find((task) => task.id === taskId) ?? null;
}

/**
 * 请求停止后台任务（POST cancel → 终态行）；响应行就地 upsert，让卡片/
 * 指示器立即收敛（轮询对账兜底）。404（已结束）忽略即可。
 */
export async function cancelBackgroundTask(taskId: string): Promise<void> {
  const { data, error } = await client.POST(
    "/api/background-tasks/{id}/cancel",
    { params: { path: { id: taskId } } },
  );
  if (!error && data != null) upsertTask(data);
}

// ---------------------------------------------------------------------------
// 轮询：存在非终态任务且页面可见时每 2s 刷新；全部终态即停（最后多守一个
// 唤醒宽限窗——宽限期内继续探测 /run，发现 running 就把流挂回来）。
// ---------------------------------------------------------------------------

function startPolling(conversationId: string) {
  pollConversationId = conversationId;
  if (pollTimer === null) {
    pollTimer = setTimeout(() => void pollTick(), POLL_INTERVAL_MS);
  }
}

function stopPolling() {
  pollConversationId = null;
  if (pollTimer !== null) {
    clearTimeout(pollTimer);
    pollTimer = null;
  }
}

/** 页面隐藏时跳过网络（定时链保留，回前台自动恢复）。node 环境恒可见。 */
function isDocumentHidden(): boolean {
  return typeof document !== "undefined" && document.hidden;
}

async function pollTick(): Promise<void> {
  pollTimer = null;
  const conversationId = pollConversationId;
  // 停止条件：会话已切走/清空（新会话的 refresh 自己会重启轮询）。
  if (!conversationId || loadedConversationId.value !== conversationId) {
    stopPolling();
    return;
  }
  const hadActive = hasActiveTask(tasks.value);
  const inWakeGrace = Date.now() < wakeGraceUntil;
  if (!hadActive && !inWakeGrace) {
    // 全部终态且宽限窗已过：停。
    stopPolling();
    return;
  }
  try {
    if (!isDocumentHidden()) {
      // 结算转换触发的宽限窗由 commitSnapshot 统一开出。
      await refreshBackgroundTasks(conversationId);
      if (Date.now() < wakeGraceUntil) {
        await probeWakeRun(conversationId);
      }
    }
  } finally {
    // 自续定时链（会话切换导致的 stopPolling 已把 pollConversationId 置空）。
    if (pollConversationId === conversationId && pollTimer === null) {
      pollTimer = setTimeout(() => void pollTick(), POLL_INTERVAL_MS);
    }
  }
}

/**
 * 唤醒 run 可见性（P1）：会话空闲时后端在任务终态后自动起 wake run，
 * 前端没有 SSE 订阅看不到流式回复——宽限窗内探测 /run，并经
 * wakeRunHandler 对账：running 挂回流，idle 重拉历史（快速 wake 已结束）。
 * 探测到 run 活跃（用户自己的 run 也算——它占着会话，wake run 还起不来）
 * 时顺延宽限窗，避免长 run 期间窗口提前过期。
 */
async function probeWakeRun(conversationId: string): Promise<void> {
  if (probingRun) return;
  probingRun = true;
  try {
    const { data, error } = await client.GET("/api/conversations/{id}/run", {
      params: { path: { id: conversationId } },
    });
    if (error || !data || loadedConversationId.value !== conversationId) return;
    if (data.running) wakeGraceUntil = Date.now() + WAKE_GRACE_MS;
    // A wake run can finish entirely between polls. The handler must also
    // reconcile idle history so its notification and reply become visible.
    if (wakeRunHandler) await wakeRunHandler(conversationId);
  } catch {
    // 网络失败静默：下一 tick 再探。
  } finally {
    probingRun = false;
  }
}

/** 注入 wake run 挂接器（App.vue 启动时接线到 attachRun；测试注入 spy）。 */
export function setWakeRunHandler(
  handler: ((conversationId: string) => unknown) | null,
): void {
  wakeRunHandler = handler;
}

// ---------------------------------------------------------------------------
// 信封卡片定位高亮（通知分隔条/底部指示器的「查看任务」）：轻量的会话级
// 单高亮，2.4s 自动消退；BashTaskCard 据此切边框/背景。
// ---------------------------------------------------------------------------

const highlightedTaskId = ref<string | null>(null);
let highlightTimer: ReturnType<typeof setTimeout> | null = null;

export function highlightBackgroundTask(taskId: string): void {
  highlightedTaskId.value = taskId;
  if (highlightTimer !== null) clearTimeout(highlightTimer);
  highlightTimer = setTimeout(() => {
    highlightedTaskId.value = null;
    highlightTimer = null;
  }, 2400);
}

export function isBackgroundTaskHighlighted(taskId: string): boolean {
  return highlightedTaskId.value === taskId;
}

// ---------------------------------------------------------------------------
// 展示助手（BashTaskCard 与底部指示器共用）
// ---------------------------------------------------------------------------

export function backgroundStatusDot(status: BackgroundTaskStatus): string {
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

export function backgroundStatusLabel(status: BackgroundTaskStatus): string {
  return (
    {
      running: "运行中",
      completed: "已完成",
      failed: "失败",
      cancelled: "已取消",
      lost: "已中断",
    }[status] ?? status
  );
}

export function backgroundStatusClass(status: BackgroundTaskStatus): string {
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

/** 1536 → "1.5 KB"；5.5 MiB → "5.5 MB"；GiB 起 "GB"。 */
export function formatBackgroundBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MB`;
  return `${(n / (1024 * 1024 * 1024)).toFixed(2)} GB`;
}

const activeCount = computed(
  () =>
    tasks.value.filter((task) => !isBackgroundTaskTerminal(task.status)).length,
);

export function useBackgroundTasks() {
  /** 当前会话的任务列表（store 已加载该会话时；否则空）。 */
  function backgroundTasksFor(conversationId: MaybeRefOrGetter<string | null>) {
    return computed(() => {
      const id = toValue(conversationId);
      if (id == null || loadedConversationId.value !== id) return [];
      return tasks.value;
    });
  }

  return {
    tasks: readonly(tasks),
    loadedConversationId: readonly(loadedConversationId),
    activeCount,
    refreshBackgroundTasks,
    cancelBackgroundTask,
    backgroundTasksFor,
    backgroundTaskById,
  };
}
