import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import client from "@/api/client";
import {
  backgroundStatusClass,
  backgroundStatusDot,
  backgroundStatusLabel,
  cancelBackgroundTask,
  formatBackgroundBytes,
  isBackgroundTaskTerminal,
  refreshBackgroundTasks,
  setWakeRunHandler,
  useBackgroundTasks,
  WAKE_GRACE_MS,
} from "@/composables/useBackgroundTasks";
import type { BackgroundTaskDto } from "@/composables/useBackgroundTasks";

// store 走 openapi-fetch client，这里整体 mock 掉（不依赖 schema.d.ts）。
// GET 按路径分发：任务列表端点与 /run 探测共用一个 mock。
vi.mock("@/api/client", () => ({
  default: { GET: vi.fn(), POST: vi.fn() },
}));

const GET = vi.mocked(client.GET);
const POST = vi.mocked(client.POST);

const LIST_PATH = "/api/conversations/{id}/background-tasks";

/** 列表端点与 /run 探测的分流 mock（非列表路径一律视为 /run 探测）；
 * 返回计数句柄供断言。 */
function mockRoutes(listRows: () => BackgroundTaskDto[], running = false) {
  const calls = { list: 0, run: 0 };
  GET.mockImplementation(async (path: string) => {
    if (path === LIST_PATH) {
      calls.list += 1;
      return { data: listRows(), error: undefined } as never;
    }
    calls.run += 1;
    return { data: { running }, error: undefined } as never;
  });
  return calls;
}

const { tasks, loadedConversationId, activeCount, backgroundTasksFor } =
  useBackgroundTasks();

const T0 = "2026-09-24T00:00:00";

function dto(
  id: string,
  overrides: Partial<BackgroundTaskDto> = {},
): BackgroundTaskDto {
  return {
    id,
    conversation_id: "conv-1",
    command: `cargo build ${id}`,
    working_dir: null,
    status: "running",
    exit_code: null,
    output_path: `/var/lib/oct/exec/conv-1/${id}.log`,
    output_bytes: 128,
    error: null,
    created_at: T0,
    finished_at: null,
    ...overrides,
  };
}

beforeEach(() => {
  GET.mockReset();
  POST.mockReset();
  setWakeRunHandler(null);
  void refreshBackgroundTasks(null);
});

// ---------------------------------------------------------------------------
// refreshBackgroundTasks：快照拉取 + 迟到响应守卫
// ---------------------------------------------------------------------------

describe("refreshBackgroundTasks", () => {
  it("gET 任务列表端点并写入快照（running 优先、created_at 升序）", async () => {
    GET.mockResolvedValue({
      data: [
        dto("t-done", {
          status: "completed",
          exit_code: 0,
          finished_at: T0,
        }),
        dto("t-run-b", { created_at: "2026-09-24T00:00:02" }),
        dto("t-run-a", { created_at: "2026-09-24T00:00:01" }),
      ],
      error: undefined,
    } as never);

    await refreshBackgroundTasks("conv-1");

    expect(GET).toHaveBeenCalledWith(LIST_PATH, {
      params: { path: { id: "conv-1" } },
    });
    expect(loadedConversationId.value).toBe("conv-1");
    expect(tasks.value.map((t) => t.id)).toEqual([
      "t-run-a",
      "t-run-b",
      "t-done",
    ]);
    expect(activeCount.value).toBe(2);
    expect(backgroundTasksFor("conv-1").value).toHaveLength(3);
    expect(backgroundTasksFor("conv-other").value).toEqual([]);
  });

  it("旧会话的迟到响应不得覆盖新会话（seq 守卫）", async () => {
    let resolveA: (value: unknown) => void = () => {};
    let resolveB: (value: unknown) => void = () => {};
    GET.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          resolveA = resolve;
        }) as never,
    );
    GET.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          resolveB = resolve;
        }) as never,
    );

    const first = refreshBackgroundTasks("conv-a");
    const second = refreshBackgroundTasks("conv-b");

    resolveA({ data: [dto("stale-a")], error: undefined });
    await first;
    expect(tasks.value.map((t) => t.id)).toEqual([]);

    resolveB({
      data: [dto("fresh-b", { conversation_id: "conv-b" })],
      error: undefined,
    });
    await second;
    expect(tasks.value.map((t) => t.id)).toEqual(["fresh-b"]);
    expect(loadedConversationId.value).toBe("conv-b");
  });

  it("conversationId 为 null 时清空任务与已加载会话", async () => {
    GET.mockResolvedValue({ data: [dto("t1")], error: undefined } as never);
    await refreshBackgroundTasks("conv-1");
    expect(tasks.value).toHaveLength(1);

    await refreshBackgroundTasks(null);
    expect(tasks.value).toHaveLength(0);
    expect(loadedConversationId.value).toBeNull();
    expect(activeCount.value).toBe(0);
  });

  it("请求失败保留旧列表", async () => {
    GET.mockResolvedValue({ data: [dto("keep")], error: undefined } as never);
    await refreshBackgroundTasks("conv-1");
    GET.mockResolvedValue({
      data: undefined,
      error: { error: "boom" },
    } as never);
    await refreshBackgroundTasks("conv-1");
    expect(tasks.value.map((t) => t.id)).toEqual(["keep"]);
  });
});

// ---------------------------------------------------------------------------
// cancelBackgroundTask：POST + 终态行 upsert
// ---------------------------------------------------------------------------

describe("cancelBackgroundTask", () => {
  it("pOST 取消端点并把终态行就地落位（running → cancelled）", async () => {
    GET.mockResolvedValue({ data: [dto("t1")], error: undefined } as never);
    await refreshBackgroundTasks("conv-1");

    POST.mockResolvedValue({
      data: dto("t1", {
        status: "cancelled",
        exit_code: null,
        finished_at: T0,
      }),
      error: undefined,
    } as never);
    await cancelBackgroundTask("t1");

    expect(POST).toHaveBeenCalledWith("/api/background-tasks/{id}/cancel", {
      params: { path: { id: "t1" } },
    });
    expect(tasks.value[0]!.status).toBe("cancelled");
    expect(activeCount.value).toBe(0);
    // upsert 保留排序形状（running 优先）
    expect(tasks.value.map((t) => t.id)).toEqual(["t1"]);
  });

  it("失败/404 不动现有快照", async () => {
    GET.mockResolvedValue({ data: [dto("t1")], error: undefined } as never);
    await refreshBackgroundTasks("conv-1");

    POST.mockResolvedValue({
      data: undefined,
      error: { error: "not found" },
    } as never);
    await cancelBackgroundTask("t1");
    expect(tasks.value[0]!.status).toBe("running");
  });
});

// ---------------------------------------------------------------------------
// 轮询：非终态任务每 2s 刷新；全部终态（含 cancel 结算）后开唤醒宽限窗
// 探测 /run，宽限过期即停。
// ---------------------------------------------------------------------------

describe("轮询启停", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("存在运行中任务时每 2s 刷新；全部终态且宽限过期后停止", async () => {
    let rows = [dto("t1")];
    const calls = mockRoutes(() => rows);

    await refreshBackgroundTasks("conv-1");
    expect(calls.list).toBe(1);

    // 运行中：每 2s 一刷
    await vi.advanceTimersByTimeAsync(2000);
    expect(calls.list).toBe(2);
    await vi.advanceTimersByTimeAsync(2000);
    expect(calls.list).toBe(3);

    // 结算：下一次刷新拿到 completed → 宽限窗开出，tick 期间探测 /run
    rows = [dto("t1", { status: "completed", exit_code: 0, finished_at: T0 })];
    await vi.advanceTimersByTimeAsync(2000);
    expect(calls.list).toBe(4);
    expect(calls.run).toBe(1);

    // 宽限期内继续轮询（refresh + /run 探测）
    await vi.advanceTimersByTimeAsync(2000);
    expect(calls.list).toBe(5);
    expect(calls.run).toBe(2);

    // 宽限过期后（结算后 30s）：tick 自停
    await vi.advanceTimersByTimeAsync(WAKE_GRACE_MS + 2 * 2000);
    const listAfterStop = calls.list;
    const runAfterStop = calls.run;
    expect(listAfterStop).toBeGreaterThan(5);

    await vi.advanceTimersByTimeAsync(10_000);
    expect(calls.list).toBe(listAfterStop);
    expect(calls.run).toBe(runAfterStop);
    expect(activeCount.value).toBe(0);
  });

  it("全终态的历史快照不启动轮询", async () => {
    const calls = mockRoutes(() => [
      dto("t-old", { status: "completed", exit_code: 0 }),
    ]);
    await refreshBackgroundTasks("conv-1");
    await vi.advanceTimersByTimeAsync(10_000);
    expect(calls.list).toBe(1);
    expect(calls.run).toBe(0);
  });

  it("会话切换后旧轮询自停（loadedConversationId 守卫）", async () => {
    const calls = mockRoutes(() => [dto("t1")]);
    await refreshBackgroundTasks("conv-1");
    expect(calls.list).toBe(1);

    // 切到无任务会话：startPolling 不会被再次调用，旧链在下一 tick 自停
    GET.mockImplementation(
      async () =>
        ({
          data: [],
          error: undefined,
        }) as never,
    );
    await refreshBackgroundTasks("conv-2");

    await vi.advanceTimersByTimeAsync(10_000);
    const afterStop = calls.list;
    await vi.advanceTimersByTimeAsync(10_000);
    expect(calls.list).toBe(afterStop);
  });

  it("refresh(null) 立即停止轮询", async () => {
    const calls = mockRoutes(() => [dto("t1")]);
    await refreshBackgroundTasks("conv-1");

    await refreshBackgroundTasks(null);
    await vi.advanceTimersByTimeAsync(10_000);
    expect(calls.list).toBe(1);
  });
});

// ---------------------------------------------------------------------------
// 唤醒 run 探测（P1）：宽限窗内 /run running → wake handler
// ---------------------------------------------------------------------------

describe("唤醒 run 探测", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("任务结算后的宽限窗内发现 running 即调用 wake handler", async () => {
    let rows = [dto("t1")];
    let runActive = false;
    let listCalls = 0;
    let runProbes = 0;
    GET.mockImplementation(async (path: string) => {
      if (path === LIST_PATH) {
        listCalls += 1;
        return { data: rows, error: undefined } as never;
      }
      runProbes += 1;
      return { data: { running: runActive }, error: undefined } as never;
    });
    const wake = vi.fn().mockResolvedValue(undefined);
    setWakeRunHandler(wake);

    await refreshBackgroundTasks("conv-1");
    await vi.advanceTimersByTimeAsync(2000);
    // 任务运行中：无宽限窗，不探测 /run、不挂接
    expect(runProbes).toBe(0);
    expect(wake).not.toHaveBeenCalled();

    // 结算（下一 tick 观测到转换开出宽限窗）+ 后端起 wake run
    rows = [dto("t1", { status: "completed", exit_code: 0, finished_at: T0 })];
    await vi.advanceTimersByTimeAsync(2000);
    expect(listCalls).toBeGreaterThanOrEqual(3);
    // 宽限窗已开出：idle 也须对账，wake run 可能已在两次轮询间结束。
    expect(runProbes).toBe(1);
    expect(wake).toHaveBeenCalledTimes(1);

    runActive = true;
    await vi.advanceTimersByTimeAsync(2000);
    expect(wake).toHaveBeenCalledTimes(2);
    expect(wake).toHaveBeenCalledWith("conv-1");
  });

  it("wake run 在两次轮询间结束时仍触发历史对账", async () => {
    let rows = [dto("t1")];
    mockRoutes(() => rows, false);
    const reconcile = vi.fn().mockResolvedValue(undefined);
    setWakeRunHandler(reconcile);
    await refreshBackgroundTasks("conv-1");
    rows = [dto("t1", { status: "completed", finished_at: T0 })];
    await vi.advanceTimersByTimeAsync(2000);
    expect(reconcile).toHaveBeenCalledWith("conv-1");
    await vi.advanceTimersByTimeAsync(WAKE_GRACE_MS + 4000);
    const calls = reconcile.mock.calls.length;
    await vi.advanceTimersByTimeAsync(4000);
    expect(reconcile).toHaveBeenCalledTimes(calls);
  });

  it("当前会话首次看到快速任务时已终态也会对账，历史终态不会开窗", async () => {
    let rows: BackgroundTaskDto[] = [];
    mockRoutes(() => rows, false);
    const reconcile = vi.fn().mockResolvedValue(undefined);
    setWakeRunHandler(reconcile);
    await refreshBackgroundTasks("conv-1");
    rows = [dto("fast", { status: "completed", finished_at: T0 })];
    await refreshBackgroundTasks("conv-1");
    await vi.advanceTimersByTimeAsync(2000);
    expect(reconcile).toHaveBeenCalledWith("conv-1");
  });

  it("cancel 结算同样开出宽限窗并探测 /run", async () => {
    let rows = [dto("t1")];
    let runProbes = 0;
    GET.mockImplementation(async (path: string) => {
      if (path === LIST_PATH) {
        return { data: rows, error: undefined } as never;
      }
      runProbes += 1;
      return { data: { running: false }, error: undefined } as never;
    });

    await refreshBackgroundTasks("conv-1");

    POST.mockResolvedValue({
      data: dto("t1", { status: "cancelled", finished_at: T0 }),
      error: undefined,
    } as never);
    await cancelBackgroundTask("t1");
    rows = [dto("t1", { status: "cancelled", finished_at: T0 })];

    await vi.advanceTimersByTimeAsync(2000);
    expect(runProbes).toBe(1);
  });

  it("探测到 running 会顺延宽限窗（run 占线期间窗口不过期）", async () => {
    let rows = [dto("t1")];
    const wake = vi.fn().mockResolvedValue(undefined);
    setWakeRunHandler(wake);
    GET.mockImplementation(async (path: string) => {
      if (path === LIST_PATH) {
        return { data: rows, error: undefined } as never;
      }
      // /run 恒活跃：宽限窗应被不断顺延而非过期
      return { data: { running: true }, error: undefined } as never;
    });

    await refreshBackgroundTasks("conv-1");
    rows = [dto("t1", { status: "completed", exit_code: 0 })];
    await vi.advanceTimersByTimeAsync(2000);

    // 结算后远超单个宽限窗的时长里探测仍在进行（wake handler 持续被调）
    await vi.advanceTimersByTimeAsync(WAKE_GRACE_MS + 4 * 2000);
    expect(wake.mock.calls.length).toBeGreaterThanOrEqual(3);
  });
});

// ---------------------------------------------------------------------------
// 展示助手
// ---------------------------------------------------------------------------

describe("状态与格式化助手", () => {
  it("终态判定：仅 running 非终态", () => {
    expect(isBackgroundTaskTerminal("running")).toBe(false);
    expect(isBackgroundTaskTerminal("completed")).toBe(true);
    expect(isBackgroundTaskTerminal("failed")).toBe(true);
    expect(isBackgroundTaskTerminal("cancelled")).toBe(true);
    expect(isBackgroundTaskTerminal("lost")).toBe(true);
  });

  it("状态点/文案/颜色按状态映射", () => {
    expect(backgroundStatusDot("running")).toBe("bg-accent-7");
    expect(backgroundStatusDot("lost")).toBe("bg-warning-10");
    expect(backgroundStatusLabel("running")).toBe("运行中");
    expect(backgroundStatusLabel("lost")).toBe("已中断");
    expect(backgroundStatusLabel("cancelled")).toBe("已取消");
    expect(backgroundStatusClass("failed")).toBe("text-danger-10");
  });

  it("字节数人类可读格式", () => {
    expect(formatBackgroundBytes(0)).toBe("0 B");
    expect(formatBackgroundBytes(1536)).toBe("1.5 KB");
    expect(formatBackgroundBytes(5.5 * 1024 * 1024)).toBe("5.5 MB");
    expect(formatBackgroundBytes(2 * 1024 * 1024 * 1024)).toBe("2.00 GB");
  });
});
