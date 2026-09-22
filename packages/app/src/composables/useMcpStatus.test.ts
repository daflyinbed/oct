import { beforeEach, describe, expect, it, vi } from "vitest";
import client from "@/api/client";
import {
  mcpStatusClass,
  mcpStatusDot,
  useMcpStatus,
} from "@/composables/useMcpStatus";

vi.mock("@/api/client", () => ({
  default: { GET: vi.fn(), POST: vi.fn(), PATCH: vi.fn(), DELETE: vi.fn() },
}));

const POST = vi.mocked(client.POST);

const { servers, ensuring, ensureServers } = useMcpStatus();

const T0 = "2026-09-21T00:00:00";

function server(
  id: string,
  overrides: Partial<{
    name: string;
    enabled: boolean;
    status: string;
    tool_count: number | null;
    error: string | null;
    project_id: string | null;
  }> = {},
) {
  return {
    id,
    name: id,
    transport: "stdio",
    command: "npx",
    args: ["-y", "@modelcontextprotocol/server-filesystem"],
    env: { API_TOKEN: "secret" },
    url: null,
    headers: null,
    enabled: true,
    tool_timeout_ms: null,
    project_id: null,
    created_at: T0,
    updated_at: T0,
    status: "not_connected",
    tool_count: null,
    error: null,
    ...overrides,
  };
}

beforeEach(() => {
  POST.mockReset();
});

describe("ensureServers", () => {
  it("pOST 到 ensure 端点并写入该项目有效集合", async () => {
    POST.mockResolvedValue({
      data: [
        server("s1", { status: "connected", tool_count: 3 }),
        server("s2", {
          project_id: "p1",
          status: "failed",
          error: "spawn failed",
        }),
      ],
      error: undefined,
    } as never);

    await ensureServers("p1");

    expect(POST).toHaveBeenCalledWith("/api/projects/{projectId}/mcp/ensure", {
      params: { path: { projectId: "p1" } },
    });
    expect(servers.value.map((s) => s.id)).toEqual(["s1", "s2"]);
    expect(ensuring.value).toBe(false);
  });

  it("请求失败时保留旧列表，ensuring 复位", async () => {
    POST.mockResolvedValue({
      data: [server("keep", { status: "connected", tool_count: 1 })],
      error: undefined,
    } as never);
    await ensureServers("p1");

    POST.mockResolvedValue({
      data: undefined,
      error: { error: "Project not found" },
    } as never);
    await ensureServers("p-missing");

    expect(servers.value.map((s) => s.id)).toEqual(["keep"]);
    expect(ensuring.value).toBe(false);
  });

  it("并发切项目时只应用最新一次的响应", async () => {
    let resolveA: (value: unknown) => void = () => {};
    let resolveB: (value: unknown) => void = () => {};
    POST.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          resolveA = resolve;
        }) as never,
    );
    POST.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          resolveB = resolve;
        }) as never,
    );

    const first = ensureServers("p-a");
    const second = ensureServers("p-b");

    // 旧项目的响应后到：不得覆盖现有列表（模块级状态在用例间共享，
    // 这里承载上一用例遗留的 keep——旧响应不覆盖它即是守卫生效）。
    resolveA({ data: [server("stale-a")], error: undefined });
    await first;
    expect(servers.value.map((s) => s.id)).toEqual(["keep"]);

    resolveB({ data: [server("fresh-b")], error: undefined });
    await second;
    expect(servers.value.map((s) => s.id)).toEqual(["fresh-b"]);
    expect(ensuring.value).toBe(false);
  });
});

describe("状态展示助手", () => {
  it("dot 颜色与语义 class 按状态映射", () => {
    expect(mcpStatusDot("connected")).toBe("bg-success-10");
    expect(mcpStatusDot("failed")).toBe("bg-danger-10");
    expect(mcpStatusDot("not_connected")).toBe("bg-neutral-5");
    expect(mcpStatusClass("connected")).toBe("text-dim");
    expect(mcpStatusClass("failed")).toBe("text-danger-10");
    expect(mcpStatusClass("not_connected")).toBe("text-faint");
  });
});
