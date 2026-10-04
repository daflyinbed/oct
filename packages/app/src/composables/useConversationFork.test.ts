import { beforeEach, expect, it, vi } from "vitest";
import client from "@/api/client";
import { useConversationFork } from "./useConversationFork";
import { useProjects } from "./useProjects";

vi.mock("@/api/client", () => ({ default: { POST: vi.fn() } }));
const POST = vi.mocked(client.POST);
const result = {
  conversation: { id: "fork-1", project_id: "p-fork", title: "分支" },
  context_revision: 4,
  cut: { kind: "head" },
  draft: null,
};
beforeEach(() => {
  POST.mockReset();
});

it("成功后把分支加入侧栏，复用响应不会重复插入", async () => {
  const session = useConversationFork();
  POST.mockResolvedValue({ data: result } as never);
  expect(await session.fork("source", 4, { kind: "head" })).toEqual(result);
  await session.fork("source", 4, { kind: "head" });
  const rows = useProjects().conversations.value.get("p-fork") ?? [];
  expect(rows.filter((row) => row.id === "fork-1")).toHaveLength(1);
  expect(POST.mock.calls[0]?.[1]).toMatchObject({
    params: { path: { id: "source" } },
    body: { expected_revision: 4, cut: { kind: "head" } },
  });
});

it("网络结果不确定时保留 operation_id，重试成功只建一个分支", async () => {
  const session = useConversationFork();
  POST.mockRejectedValueOnce(new Error("connection lost"));
  POST.mockResolvedValueOnce({ data: result } as never);
  expect(await session.fork("source", 4, { kind: "head" })).toBeNull();
  expect(session.error.value).toContain("可以重试");
  expect(session.busy.value).toBe(false);
  await session.fork("source", 4, { kind: "head" });
  expect(POST.mock.calls[0]?.[1]).toEqual(POST.mock.calls[1]?.[1]);
  expect(session.error.value).toBeNull();
});

it("版本冲突提示刷新，不悄悄重新选择切点", async () => {
  const session = useConversationFork();
  POST.mockResolvedValue({
    error: { code: "stale_revision", error: "changed" },
  } as never);
  const cut = {
    kind: "message",
    message_id: "user-2",
    side: "before",
  } as const;
  expect(await session.fork("source", 4, cut)).toBeNull();
  expect(session.error.value).toContain("刷新");
  expect(POST).toHaveBeenCalledTimes(1);
  expect(POST.mock.calls[0]?.[1]).toMatchObject({ body: { cut } });
});

it("请求在途时忽略重复点击", async () => {
  const session = useConversationFork();
  let resolve!: (value: unknown) => void;
  POST.mockImplementationOnce(
    () =>
      new Promise((done) => {
        resolve = done;
      }) as never,
  );
  const first = session.fork("source", 4, { kind: "head" });
  expect(session.busy.value).toBe(true);
  expect(await session.fork("source", 4, { kind: "head" })).toBeNull();
  expect(POST).toHaveBeenCalledTimes(1);
  resolve({ data: result });
  await first;
  expect(session.busy.value).toBe(false);
});
