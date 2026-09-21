import { beforeEach, describe, expect, it, vi } from "vitest";
import client from "@/api/client";
import { useProjects } from "@/composables/useProjects";

vi.mock("@/api/client", () => ({
  default: { GET: vi.fn(), POST: vi.fn(), DELETE: vi.fn() },
}));

const GET = vi.mocked(client.GET);

const { conversations, fetchConversations, updateConversationTitle } =
  useProjects();

function conversation(id: string, title: string) {
  return {
    id,
    project_id: "p1",
    title,
    title_source: "default",
    created_at: "2026-09-21T00:00:00",
    updated_at: "2026-09-21T00:00:00",
  };
}

function stubList(list: unknown[]) {
  GET.mockResolvedValue({ data: list, error: undefined });
}

beforeEach(() => {
  GET.mockReset();
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
