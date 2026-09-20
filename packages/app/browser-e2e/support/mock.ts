import type { CapturedRequest, Scenario, ScriptStep } from "../mock/server";

export type { CapturedRequest, Scenario, ScriptStep };

// 管理 API 与页面同源(经 vitest.browser.config.ts 的 /__mock 代理),
// 不需要 CORS。所有调用都 await 到 mock 服务器确认后返回。

export async function resetMock(): Promise<void> {
  await fetch("/__mock/reset", { method: "POST" });
}

export async function setScenario(scenario: Scenario): Promise<void> {
  await fetch("/__mock/scenario", {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(scenario),
  });
}

/** 放行剧本里的一个 gate(幂等;流式断言的确定性同步原语)。 */
export async function openGate(name: string): Promise<void> {
  await fetch(`/__mock/gates/${encodeURIComponent(name)}/open`, {
    method: "POST",
  });
}

export async function getRequests(): Promise<CapturedRequest[]> {
  const res = await fetch("/__mock/requests");
  return (await res.json()) as CapturedRequest[];
}
