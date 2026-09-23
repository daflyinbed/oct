import { beforeEach, describe, expect, it, vi } from "vitest";
import client from "@/api/client";
import {
  mcpOauthState,
  readMcpResource,
  renderMcpPrompt,
  useMcpServers,
} from "@/composables/useMcpServers";

// ---------------------------------------------------------------------------
// renderMcpPrompt / readMcpResource（瞬时调用，无本地状态）
// ---------------------------------------------------------------------------

vi.mock("@/api/client", () => ({
  default: { GET: vi.fn(), POST: vi.fn(), PATCH: vi.fn(), DELETE: vi.fn() },
}));

const GET = vi.mocked(client.GET);
const POST = vi.mocked(client.POST);
const PATCH = vi.mocked(client.PATCH);
const DELETE = vi.mocked(client.DELETE);

const {
  servers,
  error,
  fetchServers,
  createServer,
  updateServer,
  deleteServer,
  toggleEnabled,
  clearError,
  startOauthLogin,
  waitForOauth,
  logoutOauth,
} = useMcpServers();

const T0 = "2026-09-21T00:00:00";

function oauthSummary(
  overrides: Partial<{
    has_token: boolean;
    has_refresh_token: boolean;
    expires_at: string | null;
  }> = {},
) {
  return {
    has_token: false,
    has_refresh_token: false,
    expires_at: null,
    ...overrides,
  };
}

function server(
  id: string,
  overrides: Partial<{
    name: string;
    transport: string;
    url: string | null;
    command: string;
    enabled: boolean;
    status: string;
    tool_count: number | null;
    project_id: string | null;
    oauth: ReturnType<typeof oauthSummary>;
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
    oauth: oauthSummary(),
    ...overrides,
  };
}

beforeEach(() => {
  GET.mockReset();
  POST.mockReset();
  PATCH.mockReset();
  DELETE.mockReset();
});

describe("fetchServers", () => {
  it("无参拉全量列表填充状态", async () => {
    GET.mockResolvedValue({
      data: [server("s1"), server("s2", { name: "fs" })],
      error: undefined,
    } as never);

    await fetchServers();

    expect(GET).toHaveBeenCalledWith("/api/mcp/servers", {
      params: undefined,
    });
    expect(servers.value.map((s) => s.id)).toEqual(["s1", "s2"]);
  });

  it("带 project_id 查询该项目有效集合", async () => {
    GET.mockResolvedValue({ data: [], error: undefined } as never);

    await fetchServers("p1");

    expect(GET).toHaveBeenCalledWith("/api/mcp/servers", {
      params: { query: { project_id: "p1" } },
    });
  });
});

describe("createServer", () => {
  it("成功后把返回行（not_connected）追加进列表", async () => {
    GET.mockResolvedValue({ data: [server("s1")], error: undefined } as never);
    await fetchServers();
    POST.mockResolvedValue({
      data: server("s-new", { name: "fs", status: "not_connected" }),
      error: undefined,
    } as never);

    const created = await createServer({
      name: "fs",
      transport: "stdio",
      command: "npx",
    });

    expect(POST).toHaveBeenCalledWith("/api/mcp/servers", {
      body: { name: "fs", transport: "stdio", command: "npx" },
    });
    expect(created?.id).toBe("s-new");
    expect(servers.value.map((s) => s.id)).toEqual(["s1", "s-new"]);
  });

  it("409 重名时透出服务端错误信息，列表不变", async () => {
    GET.mockResolvedValue({ data: [server("s1")], error: undefined } as never);
    await fetchServers();
    POST.mockResolvedValue({
      data: undefined,
      error: { error: "MCP server 's1' already exists" },
    } as never);

    const created = await createServer({
      name: "s1",
      transport: "stdio",
      command: "npx",
    });

    expect(created).toBeNull();
    expect(error.value).toBe("MCP server 's1' already exists");
    expect(servers.value).toHaveLength(1);
  });
});

describe("updateServer", () => {
  it("pATCH 成功后重 GET，以服务端结果就地替换条目", async () => {
    GET.mockResolvedValue({
      data: [server("s1", { enabled: true })],
      error: undefined,
    } as never);
    await fetchServers();

    PATCH.mockResolvedValue({ data: undefined, error: undefined } as never);
    GET.mockResolvedValue({
      data: server("s1", {
        command: "uvx",
        status: "connected",
        tool_count: 4,
      }),
      error: undefined,
    } as never);

    const ok = await updateServer("s1", { command: "uvx" });

    expect(ok).toBe(true);
    expect(PATCH).toHaveBeenCalledWith("/api/mcp/servers/{id}", {
      params: { path: { id: "s1" } },
      body: { command: "uvx" },
    });
    const updated = servers.value.find((s) => s.id === "s1");
    expect(updated?.command).toBe("uvx");
    expect(updated?.status).toBe("connected");
    expect(updated?.tool_count).toBe(4);
  });

  it("pATCH 409 时透出错误、返回 false，不发重 GET", async () => {
    GET.mockResolvedValue({
      data: [server("s1", { name: "old" })],
      error: undefined,
    } as never);
    await fetchServers();
    GET.mockClear();

    PATCH.mockResolvedValue({
      data: undefined,
      error: { error: "MCP server 'dup' already exists" },
    } as never);

    const ok = await updateServer("s1", { name: "dup" });

    expect(ok).toBe(false);
    expect(error.value).toBe("MCP server 'dup' already exists");
    expect(servers.value[0]?.name).toBe("old");
    expect(GET).not.toHaveBeenCalled();
  });
});

describe("toggleEnabled", () => {
  it("乐观翻转 enabled，PATCH 成功后以重 GET 结果对齐", async () => {
    GET.mockResolvedValue({
      data: [server("s1", { enabled: true })],
      error: undefined,
    } as never);
    await fetchServers();

    PATCH.mockResolvedValue({ data: undefined, error: undefined } as never);
    GET.mockResolvedValue({
      data: server("s1", { enabled: false, status: "not_connected" }),
      error: undefined,
    } as never);

    const ok = await toggleEnabled(servers.value[0]!, false);

    expect(ok).toBe(true);
    expect(PATCH).toHaveBeenCalledWith("/api/mcp/servers/{id}", {
      params: { path: { id: "s1" } },
      body: { enabled: false },
    });
    expect(servers.value[0]?.enabled).toBe(false);
  });

  it("pATCH 失败时回滚为提交前的值", async () => {
    GET.mockResolvedValue({
      data: [server("s1", { enabled: false })],
      error: undefined,
    } as never);
    await fetchServers();

    PATCH.mockResolvedValue({
      data: undefined,
      error: { error: "MCP server not found" },
    } as never);

    const ok = await toggleEnabled(servers.value[0]!, true);

    expect(ok).toBe(false);
    expect(servers.value[0]?.enabled).toBe(false);
  });
});

describe("deleteServer", () => {
  it("成功后就地移除条目", async () => {
    GET.mockResolvedValue({
      data: [server("s1"), server("s2")],
      error: undefined,
    } as never);
    await fetchServers();

    DELETE.mockResolvedValue({ data: undefined, error: undefined } as never);

    const ok = await deleteServer("s1");

    expect(ok).toBe(true);
    expect(servers.value.map((s) => s.id)).toEqual(["s2"]);
  });
});

describe("clearError", () => {
  it("清空错误状态（表单接管展示后调用，错误不残留）", async () => {
    GET.mockResolvedValue({ data: [server("s1")], error: undefined } as never);
    await fetchServers();
    POST.mockResolvedValue({
      data: undefined,
      error: { error: "MCP server 's1' already exists" },
    } as never);
    await createServer({ name: "s1", transport: "stdio", command: "npx" });
    expect(error.value).not.toBeNull();

    clearError();

    expect(error.value).toBeNull();
    // 只清错误，不动列表
    expect(servers.value).toHaveLength(1);
  });
});

describe("renderMcpPrompt", () => {
  it("pOST 渲染端点，路径参数与 body 原样传递，返回 messages", async () => {
    POST.mockResolvedValue({
      data: { messages: [{ role: "user", text: "Greet Ada" }] },
      error: undefined,
    } as never);

    const result = await renderMcpPrompt("s1", "greet", { name: "Ada" });

    expect(POST).toHaveBeenCalledWith("/api/mcp/servers/{id}/prompts/{name}", {
      params: { path: { id: "s1", name: "greet" } },
      body: { arguments: { name: "Ada" } },
    });
    expect(result.messages).toEqual([{ role: "user", text: "Greet Ada" }]);
  });

  it("失败时抛出携带后端 error 文案的 Error", async () => {
    POST.mockResolvedValue({
      data: undefined,
      error: { error: "MCP server not connected or prompt `x` not found" },
    } as never);

    await expect(renderMcpPrompt("s1", "x", {})).rejects.toThrow(
      "MCP server not connected or prompt `x` not found",
    );
  });
});

describe("readMcpResource", () => {
  it("pOST 读取端点，返回文本内容", async () => {
    POST.mockResolvedValue({
      data: { mime_type: "text/markdown", text: "# Notes", blob_base64: null },
      error: undefined,
    } as never);

    const content = await readMcpResource("s1", "memo://notes");

    expect(POST).toHaveBeenCalledWith("/api/mcp/servers/{id}/resources/read", {
      params: { path: { id: "s1" } },
      body: { uri: "memo://notes" },
    });
    expect(content.text).toBe("# Notes");
  });

  it("失败时抛出携带后端 error 文案的 Error", async () => {
    POST.mockResolvedValue({
      data: undefined,
      error: { error: "reading resource `nope://x` failed: boom" },
    } as never);

    await expect(readMcpResource("s1", "nope://x")).rejects.toThrow(
      "reading resource `nope://x` failed: boom",
    );
  });
});

// ---------------------------------------------------------------------------
// OAuth（http transport）：start / 轮询就绪 / 退出登录 / 徽章状态派生
// ---------------------------------------------------------------------------

describe("startOauthLogin", () => {
  it("pOST start 并返回 authorization_url", async () => {
    POST.mockResolvedValue({
      data: { authorization_url: "https://as.example/authorize?state=xyz" },
      error: undefined,
    } as never);

    const url = await startOauthLogin("s1");

    expect(POST).toHaveBeenCalledWith("/api/mcp/servers/{id}/oauth/start", {
      params: { path: { id: "s1" } },
    });
    expect(url).toBe("https://as.example/authorize?state=xyz");
  });

  it("502（发现/注册失败）时抛出携带后端 error 文案的 Error", async () => {
    POST.mockResolvedValue({
      data: undefined,
      error: { error: "OAuth discovery failed: no metadata found" },
    } as never);

    await expect(startOauthLogin("s1")).rejects.toThrow(
      "OAuth discovery failed: no metadata found",
    );
  });
});

describe("waitForOauth", () => {
  it("轮询详情直到 has_token，并把最终行同步进列表", async () => {
    GET.mockResolvedValueOnce({
      data: [
        server("s1", { transport: "http", url: "https://mcp.example/mcp" }),
      ],
      error: undefined,
    } as never);
    await fetchServers();

    // 第一次未就绪，第二次就绪（回调已把 token 入库）
    GET.mockResolvedValueOnce({
      data: server("s1", { transport: "http" }),
      error: undefined,
    } as never);
    GET.mockResolvedValueOnce({
      data: server("s1", {
        transport: "http",
        status: "connected",
        oauth: oauthSummary({ has_token: true, has_refresh_token: true }),
      }),
      error: undefined,
    } as never);

    const ready = await waitForOauth("s1", { intervalMs: 1, timeoutMs: 1_000 });

    expect(ready).toBe(true);
    expect(GET).toHaveBeenCalledWith("/api/mcp/servers/{id}", {
      params: { path: { id: "s1" } },
    });
    expect(servers.value[0]?.oauth.has_token).toBe(true);
  });

  it("超时返回 false（不再无限轮询）", async () => {
    GET.mockResolvedValueOnce({
      data: [server("s1")],
      error: undefined,
    } as never);
    await fetchServers();
    // 后续所有详情轮询都未就绪
    GET.mockResolvedValue({ data: server("s1"), error: undefined } as never);

    // 25ms 预算、10ms 间隔：真实计时器下最多三轮即到点（轮询节奏是被测
    // 代码自身的行为，非测试同步 sleep）。
    const result = await waitForOauth("s1", { intervalMs: 10, timeoutMs: 25 });

    expect(result).toBe(false);
    expect(GET.mock.calls.length).toBeGreaterThanOrEqual(2);
  });

  it("详情 404（行被删）立即放弃返回 false", async () => {
    GET.mockResolvedValue({
      data: undefined,
      error: { error: "not found" },
    } as never);

    await expect(waitForOauth("gone", { intervalMs: 1 })).resolves.toBe(false);
    expect(GET).toHaveBeenCalledTimes(1);
  });
});

describe("logoutOauth", () => {
  it("dELETE 凭据行后重 GET，本地条目回到未授权概要", async () => {
    GET.mockResolvedValueOnce({
      data: [
        server("s1", {
          transport: "http",
          oauth: oauthSummary({ has_token: true }),
        }),
      ],
      error: undefined,
    } as never);
    await fetchServers();

    DELETE.mockResolvedValue({ data: undefined, error: undefined } as never);
    GET.mockResolvedValueOnce({
      data: server("s1", { transport: "http" }),
      error: undefined,
    } as never);

    const ok = await logoutOauth("s1");

    expect(ok).toBe(true);
    expect(DELETE).toHaveBeenCalledWith("/api/mcp/servers/{id}/oauth", {
      params: { path: { id: "s1" } },
    });
    expect(servers.value[0]?.oauth.has_token).toBe(false);
  });

  it("dELETE 失败透出错误、返回 false", async () => {
    DELETE.mockResolvedValue({
      data: undefined,
      error: { error: "MCP server not found" },
    } as never);

    const ok = await logoutOauth("gone");

    expect(ok).toBe(false);
    expect(error.value).toBe("MCP server not found");
  });
});

describe("mcpOauthState", () => {
  it("无凭据 → absent", () => {
    expect(mcpOauthState(server("s1"))).toEqual({ kind: "absent" });
  });

  it("有凭据且未过期 → authorized（含解析出的 UTC 过期时间）", () => {
    const future = new Date(Date.now() + 60_000);
    const iso = future.toISOString().replace("Z", "");
    const state = mcpOauthState(
      server("s1", {
        oauth: oauthSummary({ has_token: true, expires_at: iso }),
      }),
    );
    expect(state.kind).toBe("authorized");
    if (state.kind === "authorized") {
      // NaiveDateTime 无时区后缀但按 UTC 存储补 Z 解析，毫秒级截断容差
      expect(
        Math.abs((state.expiresAt?.getTime() ?? 0) - future.getTime()),
      ).toBeLessThan(1000);
    }
  });

  it("expires_at 为 null → authorized 且视为长期有效", () => {
    const state = mcpOauthState(
      server("s1", {
        oauth: oauthSummary({ has_token: true, expires_at: null }),
      }),
    );
    expect(state).toEqual({ kind: "authorized", expiresAt: null });
  });

  it("过期时间已过 → expired（引导重新登录）", () => {
    const state = mcpOauthState(
      server("s1", {
        oauth: oauthSummary({
          has_token: true,
          expires_at: "2000-01-01T00:00:00",
        }),
      }),
    );
    expect(state).toEqual({ kind: "expired" });
  });
});
