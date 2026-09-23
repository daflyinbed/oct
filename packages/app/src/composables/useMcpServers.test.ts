import { beforeEach, describe, expect, it, vi } from "vitest";
import client from "@/api/client";
import {
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
} = useMcpServers();

const T0 = "2026-09-21T00:00:00";

function server(
  id: string,
  overrides: Partial<{
    name: string;
    command: string;
    enabled: boolean;
    status: string;
    tool_count: number | null;
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
