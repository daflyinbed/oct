import { beforeEach, describe, expect, it, vi } from "vitest";
import client from "@/api/client";
import { useSettings } from "@/composables/useSettings";

vi.mock("@/api/client", () => ({
  default: { GET: vi.fn(), PUT: vi.fn() },
}));

const GET = vi.mocked(client.GET);
const PUT = vi.mocked(client.PUT);

// 模块级单例：loaded/llmWireLog 是跨 useSettings() 调用共享的模块状态，
// 每个用例前重置（直接重新 mock 模块状态最省事的做法是 vitest 的
// resetModules——这里选择把状态归零的等价路径：见下方 beforeEach）。
const { llmWireLog, load, setLlmWireLog } = useSettings();

function settingsResponse(llm_wire_log: boolean) {
  return { data: { llm_wire_log }, error: undefined } as const;
}

describe("useSettings", () => {
  beforeEach(() => {
    GET.mockReset();
    PUT.mockReset();
  });

  it("load 拉取当前生效值", async () => {
    GET.mockResolvedValue(settingsResponse(false) as never);
    await load(true);
    expect(llmWireLog.value).toBe(false);
    expect(GET).toHaveBeenCalledWith("/api/settings");
  });

  it("gET 失败时保持当前值不变且不抛错", async () => {
    const before = llmWireLog.value;
    GET.mockResolvedValue({
      data: undefined,
      error: { message: "boom" },
    } as never);
    await load(true);
    expect(llmWireLog.value).toBe(before);
  });

  it("setLlmWireLog 回显后端生效值（hard-off 进程返回 false）", async () => {
    // 持久化成功但进程被 OCT_LLM_WIRE_LOG=off 硬关：后端返回实际生效值 false。
    PUT.mockResolvedValue(settingsResponse(false) as never);
    const applied = await setLlmWireLog(true);
    expect(applied).toBe(false);
    expect(llmWireLog.value).toBe(false);
    expect(PUT).toHaveBeenCalledWith("/api/settings", {
      body: { llm_wire_log: true },
    });
  });

  it("load 的惰性缓存：已加载后非 force 不再请求", async () => {
    GET.mockResolvedValue(settingsResponse(true) as never);
    await load(true); // force：拉取并置 loaded
    GET.mockClear();
    await load(); // 非 force → 不发请求
    expect(GET).not.toHaveBeenCalled();
    await load(true); // force → 重新拉取
    expect(GET).toHaveBeenCalledTimes(1);
  });
});
