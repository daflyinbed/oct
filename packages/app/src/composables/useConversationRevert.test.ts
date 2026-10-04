import { beforeEach, expect, it, vi } from "vitest";
import client from "@/api/client";
import { useConversationRevert } from "./useConversationRevert";
import { useProjects } from "./useProjects";

vi.mock("@/api/client", () => ({ default: { POST: vi.fn() } }));
const POST = vi.mocked(client.POST);
const cut = { kind: "message", message_id: "u2", side: "before" } as const;
const result = {
  conversation: { id: "source", project_id: "p-revert", title: "original" },
  backup: {
    id: "backup",
    project_id: "p-revert",
    title: "saved",
    origin_kind: "revert_backup",
  },
  backup_conversation_id: "backup",
  context_revision: 5,
  cut,
  draft: "edit me",
};
beforeEach(() => POST.mockReset());

it("成功把原会话和只读备份更新到侧栏，重放不重复插入", async () => {
  POST.mockResolvedValue({ data: result } as never);
  const session = useConversationRevert();
  expect(await session.revert("source", 4, cut)).toEqual(result);
  await session.revert("source", 4, cut);
  expect(useProjects().conversations.value.get("p-revert")).toHaveLength(2);
  expect(POST.mock.calls[0]?.[1]).toMatchObject({
    params: { path: { id: "source" } },
    body: { expected_revision: 4, cut },
  });
});

it("结果不确定时复用 operation_id，避免重复备份和重复回退", async () => {
  POST.mockRejectedValueOnce(new Error("lost response"));
  POST.mockResolvedValueOnce({ data: result } as never);
  const session = useConversationRevert();
  expect(await session.revert("source", 4, cut)).toBeNull();
  expect(session.error.value).toContain("可以重试");
  await session.revert("source", 4, cut);
  expect(POST.mock.calls[0]?.[1]).toEqual(POST.mock.calls[1]?.[1]);
  expect(session.error.value).toBeNull();
});

it.each([
  ["stale_revision", "刷新"],
  ["background_pending", "后台任务"],
  ["already_running", "运行"],
])("明确显示 %s 的阻塞原因", async (code, hint) => {
  POST.mockResolvedValue({ error: { code, error: "blocked" } } as never);
  const session = useConversationRevert();
  expect(await session.revert("source", 4, cut)).toBeNull();
  expect(session.error.value).toContain(hint);
  expect(POST).toHaveBeenCalledTimes(1);
});
