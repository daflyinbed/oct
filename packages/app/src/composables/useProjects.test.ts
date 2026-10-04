import { beforeEach, describe, expect, it, vi } from "vitest";
import client from "@/api/client";
import {
  normalizePermissionMode,
  permissionModeBadge,
  useProjects,
} from "@/composables/useProjects";

vi.mock("@/api/client", () => ({
  default: { GET: vi.fn(), POST: vi.fn(), PATCH: vi.fn(), DELETE: vi.fn() },
}));

const GET = vi.mocked(client.GET);
const POST = vi.mocked(client.POST);
const PATCH = vi.mocked(client.PATCH);
const DELETE = vi.mocked(client.DELETE);

const {
  projects,
  conversations,
  fetchConversations,
  createProject,
  updateProject,
  deleteConversation,
  updateConversationTitle,
  renameConversation,
  setConversationPermissionMode,
} = useProjects();

function conversation(id: string, title: string) {
  return {
    id,
    project_id: "p1",
    title,
    title_source: "default",
    permission_mode: "default",
    created_at: "2026-09-21T00:00:00",
    updated_at: "2026-09-21T00:00:00",
  };
}

const T0 = "2026-09-21T00:00:00";

function project(
  id: string,
  overrides: Partial<{
    name: string;
    working_dir: string;
    additional_dirs: string[];
  }> = {},
) {
  return {
    id,
    name: id,
    working_dir: `/tmp/${id}`,
    additional_dirs: [],
    created_at: T0,
    updated_at: T0,
    ...overrides,
  };
}

function stubList(list: unknown[]) {
  // openapi-fetch 的 GET 是按路径的重载集合，vi.mocked 只取其中一个签名，
  // 与实际请求的端点对不上——按仓库惯例整体 as never 绕开。
  GET.mockResolvedValue({ data: list, error: undefined } as never);
}

beforeEach(() => {
  GET.mockReset();
  POST.mockReset();
  PATCH.mockReset();
  DELETE.mockReset();
});

describe("createProject / updateProject（附加目录）", () => {
  it("带附加目录时透传 additional_dirs，成功后追加进项目列表", async () => {
    POST.mockResolvedValue({
      data: project("p-create", { additional_dirs: ["/ext"] }),
      error: undefined,
    } as never);

    const created = await createProject("multi", "/tmp/multi", ["/ext"]);

    expect(POST).toHaveBeenCalledWith("/api/projects", {
      body: {
        name: "multi",
        working_dir: "/tmp/multi",
        additional_dirs: ["/ext"],
      },
    });
    expect(created?.id).toBe("p-create");
    expect(projects.value.some((p) => p.id === "p-create")).toBe(true);
  });

  it("不传附加目录时请求体不带该字段", async () => {
    POST.mockResolvedValue({
      data: project("p-solo"),
      error: undefined,
    } as never);

    await createProject("solo", "/tmp/solo");

    const body = (POST.mock.calls[0]?.[1] as unknown as { body: object })
      ?.body as Record<string, unknown>;
    expect("additional_dirs" in body).toBe(false);
  });

  it("updateProject PATCH 成功后重 GET，以服务端归一化结果替换本地条目", async () => {
    POST.mockResolvedValue({
      data: project("p-edit", { name: "old" }),
      error: undefined,
    } as never);
    await createProject("old", "/tmp/p-edit");

    PATCH.mockResolvedValue({ data: undefined, error: undefined } as never);
    // 服务端归一化：去重、剔除与主目录相同的条目。
    GET.mockResolvedValue({
      data: project("p-edit", { name: "new", additional_dirs: ["/ext"] }),
      error: undefined,
    } as never);

    const ok = await updateProject("p-edit", {
      name: "new",
      additional_dirs: ["/ext", "/ext", "/tmp/p-edit"],
    });

    expect(ok).toBe(true);
    expect(PATCH).toHaveBeenCalledWith("/api/projects/{id}", {
      params: { path: { id: "p-edit" } },
      body: { name: "new", additional_dirs: ["/ext", "/ext", "/tmp/p-edit"] },
    });
    const updated = projects.value.find((p) => p.id === "p-edit");
    expect(updated?.name).toBe("new");
    expect(updated?.additional_dirs).toEqual(["/ext"]);
  });

  it("updateProject PATCH 失败返回 false，本地列表不变", async () => {
    POST.mockResolvedValue({
      data: project("p-fail", { name: "keep" }),
      error: undefined,
    } as never);
    await createProject("keep", "/tmp/p-fail");

    PATCH.mockResolvedValue({ data: undefined, error: {} } as never);

    const ok = await updateProject("p-fail", { name: "x" });

    expect(ok).toBe(false);
    expect(projects.value.find((p) => p.id === "p-fail")?.name).toBe("keep");
    expect(GET).not.toHaveBeenCalled();
  });
});

describe("deleteConversation（分支来源）", () => {
  const source = conversation("source", "Source");
  const fork = {
    ...conversation("fork", "Fork"),
    forked_from_conversation_id: "source",
    forked_at_message_id: "cut-message",
    forked_at_revision: 4,
  };

  it("删除来源后清除所有直属分支的链接，保留分支标识及其他来源", async () => {
    const sibling = { ...fork, id: "sibling" };
    const nested = {
      ...fork,
      id: "nested",
      forked_from_conversation_id: "fork",
    };
    stubList([source, fork, sibling, nested]);
    await fetchConversations("p1");
    DELETE.mockResolvedValue({ error: undefined } as never);
    const before = conversations.value;

    await deleteConversation("p1", "source");

    expect(DELETE).toHaveBeenCalledWith(
      "/api/projects/{projectId}/conversations/{id}",
      { params: { path: { projectId: "p1", id: "source" } } },
    );
    expect(conversations.value).not.toBe(before);
    expect(conversations.value.get("p1")).toEqual([
      { ...fork, forked_from_conversation_id: null },
      { ...sibling, forked_from_conversation_id: null },
      nested,
    ]);
  });

  it("删除失败时保留来源及分支链接", async () => {
    stubList([source, fork]);
    await fetchConversations("p1");
    DELETE.mockResolvedValue({ error: {} } as never);
    const before = conversations.value;

    await deleteConversation("p1", "source");

    expect(conversations.value).toBe(before);
    expect(conversations.value.get("p1")).toEqual([source, fork]);
  });
});

describe("updateConversationTitle（title_updated 事件落地）", () => {
  it("本地更新命中会话的标题，map 引用整体替换保持响应式", async () => {
    stubList([
      conversation("c1", "New conversation"),
      conversation("c2", "手命名"),
    ]);
    await fetchConversations("p1");

    updateConversationTitle("c1", "AI 生成的新标题");

    const list = conversations.value.get("p1") ?? [];
    expect(list.map((c) => [c.id, c.title])).toEqual([
      ["c1", "AI 生成的新标题"],
      ["c2", "手命名"],
    ]);
  });

  it("重复事件覆盖即幂等；未加载的会话忽略", async () => {
    stubList([conversation("c1", "New conversation")]);
    await fetchConversations("p1");

    updateConversationTitle("c1", "第一次");
    updateConversationTitle("c1", "第一次");
    expect(conversations.value.get("p1")?.[0]?.title).toBe("第一次");

    updateConversationTitle("not-loaded", "不存在");
    expect(conversations.value.get("p1")).toHaveLength(1);
  });
});

describe("renameConversation（侧栏行内改名）", () => {
  it("trim 后以正确 body 调 PATCH，成功后本地标题与 title_source 一起更新为 user", async () => {
    stubList([conversation("c1", "New conversation")]);
    await fetchConversations("p1");

    PATCH.mockResolvedValue({ data: undefined, error: undefined } as never);

    const ok = await renameConversation("p1", "c1", "  手动命名  ");

    expect(ok).toBe(true);
    expect(PATCH).toHaveBeenCalledWith(
      "/api/projects/{projectId}/conversations/{id}",
      {
        params: { path: { projectId: "p1", id: "c1" } },
        body: { title: "手动命名" },
      },
    );
    const renamed = conversations.value.get("p1")?.[0];
    expect(renamed?.title).toBe("手动命名");
    expect(renamed?.title_source).toBe("user");
  });

  it("空白标题直接返回 false，不发请求、本地不变", async () => {
    stubList([conversation("c1", "New conversation")]);
    await fetchConversations("p1");

    const ok = await renameConversation("p1", "c1", "   ");

    expect(ok).toBe(false);
    expect(PATCH).not.toHaveBeenCalled();
    expect(conversations.value.get("p1")?.[0]?.title).toBe("New conversation");
    expect(conversations.value.get("p1")?.[0]?.title_source).toBe("default");
  });

  it("renameConversation PATCH 失败返回 false，本地标题保持不变", async () => {
    stubList([conversation("c1", "old")]);
    await fetchConversations("p1");

    PATCH.mockResolvedValue({ data: undefined, error: {} } as never);

    const ok = await renameConversation("p1", "c1", "new");

    expect(ok).toBe(false);
    expect(conversations.value.get("p1")?.[0]?.title).toBe("old");
    expect(conversations.value.get("p1")?.[0]?.title_source).toBe("default");
  });
});

describe("权限模式", () => {
  it("setConversationPermissionMode：PATCH 正确端点/body，成功后本地列表同步档位", async () => {
    stubList([conversation("c1", "New conversation")]);
    await fetchConversations("p1");

    PATCH.mockResolvedValue({ data: undefined, error: undefined } as never);

    const ok = await setConversationPermissionMode("c1", "accept_edits");

    expect(ok).toBe(true);
    expect(PATCH).toHaveBeenCalledWith(
      "/api/projects/{projectId}/conversations/{id}",
      {
        params: { path: { projectId: "p1", id: "c1" } },
        body: { permission_mode: "accept_edits" },
      },
    );
    expect(conversations.value.get("p1")?.[0]?.permission_mode).toBe(
      "accept_edits",
    );
  });

  it("权限模式 PATCH 失败返回 false，本地档位不变；未加载的会话也返回 false", async () => {
    stubList([conversation("c1", "New conversation")]);
    await fetchConversations("p1");

    PATCH.mockResolvedValue({ data: undefined, error: {} } as never);
    expect(await setConversationPermissionMode("c1", "bypass")).toBe(false);
    expect(conversations.value.get("p1")?.[0]?.permission_mode).toBe("default");

    expect(await setConversationPermissionMode("not-loaded", "bypass")).toBe(
      false,
    );
    expect(PATCH).toHaveBeenCalledTimes(1);
  });

  it("normalizePermissionMode 归一化；permissionModeBadge 仅非 default 有档位标", () => {
    expect(normalizePermissionMode("accept_edits")).toBe("accept_edits");
    expect(normalizePermissionMode("bypass")).toBe("bypass");
    expect(normalizePermissionMode("default")).toBe("default");
    expect(normalizePermissionMode("nonsense")).toBe("default");
    expect(normalizePermissionMode(null)).toBe("default");
    expect(normalizePermissionMode(undefined)).toBe("default");

    expect(permissionModeBadge("default")).toBeNull();
    expect(permissionModeBadge(null)).toBeNull();
    expect(permissionModeBadge("accept_edits")?.label).toBe("自动编辑");
    expect(permissionModeBadge("bypass")?.label).toBe("全放行");
  });
});

it("回退响应插入备份后，较早开始的会话列表请求不能覆盖新元数据", async () => {
  const store = useProjects();
  let resolveOld!: (value: unknown) => void;
  GET.mockImplementationOnce(
    () =>
      new Promise((resolve) => {
        resolveOld = resolve;
      }) as never,
  );
  const request = store.fetchConversations("p-revert-race");
  const original = {
    id: "source-race",
    project_id: "p-revert-race",
    title: "current",
    history_epoch: 1,
  };
  const backup = {
    id: "backup-race",
    project_id: "p-revert-race",
    title: "backup",
    origin_kind: "revert_backup",
  };
  store.upsertConversation(original as never);
  store.upsertConversation(backup as never);
  resolveOld({ data: [{ ...original, history_epoch: 0 }] });
  await request;
  expect(store.conversations.value.get("p-revert-race")).toEqual([
    backup,
    original,
  ]);
});
