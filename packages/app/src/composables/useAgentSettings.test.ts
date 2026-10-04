import { beforeEach, expect, it, vi } from "vitest";
import client from "@/api/client";
import { useAgentSettings } from "@/composables/useAgentSettings";
import type { AgentView } from "@/composables/useAgentSettings";

vi.mock("@/api/client", () => ({ default: { GET: vi.fn(), PATCH: vi.fn() } }));
const GET = vi.mocked(client.GET);
const PATCH = vi.mocked(client.PATCH);

function view(instructions: string): AgentView {
  return {
    key: "worker",
    name: "Worker",
    description: "",
    delegatable: true,
    tools_editable: true,
    overrides: { instructions },
    effective: {
      model: { mode: "parent" },
      instructions,
      tools: { mode: "parent" },
    },
    global_effective: {
      model: { mode: "parent" },
      instructions: "global",
      tools: { mode: "parent" },
    },
    sources: { model: "builtin", instructions: "project", tools: "builtin" },
  };
}
function result<T>(data: T) {
  return { data, error: undefined } as never;
}
function defer<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((r) => {
    resolve = r;
  });
  return { promise, resolve };
}
function loadWith(instructions: string) {
  GET.mockResolvedValueOnce(result([view(instructions)]));
  GET.mockResolvedValueOnce(result([]));
  GET.mockResolvedValueOnce(result([]));
}

beforeEach(() => {
  GET.mockReset();
  PATCH.mockReset();
});

it("慢的全局响应不会覆盖已经切换到的项目配置", async () => {
  const pending = defer<never>();
  GET.mockReturnValueOnce(pending.promise);
  GET.mockResolvedValueOnce(result([]));
  GET.mockResolvedValueOnce(result([]));
  const editor = useAgentSettings();
  const oldLoad = editor.load(null);
  loadWith("project");
  await editor.load("p1");
  pending.resolve(result([view("old global")]));
  await oldLoad;
  expect(editor.views.value[0]?.effective.instructions).toBe("project");
  expect(editor.loading.value).toBe(false);
  expect(GET).toHaveBeenCalledWith("/api/projects/{id}/settings/agents", {
    params: { path: { id: "p1" } },
  });
});

it("项目保存保留 null/空列表语义，服务端拒绝时保留原生效值", async () => {
  const editor = useAgentSettings();
  loadWith("before");
  await editor.load("p1");
  const body = {
    model: null,
    instructions: "",
    tools: { mode: "allowlist" as const, items: [] },
  };
  PATCH.mockResolvedValueOnce({ error: { error: "model deleted" } } as never);
  expect(await editor.save("worker", body)).toBe(false);
  expect(editor.views.value[0]?.effective.instructions).toBe("before");
  expect(editor.error.value).toBe("model deleted");
  expect(PATCH).toHaveBeenCalledWith(
    "/api/projects/{id}/settings/agents/{key}",
    { params: { path: { id: "p1", key: "worker" } }, body },
  );
  PATCH.mockResolvedValueOnce(result(view("")));
  expect(await editor.save("worker", body)).toBe(true);
  expect(editor.error.value).toBe("");
  expect(editor.views.value[0]?.effective.instructions).toBe("");
});

it("切换作用域或关闭编辑器后，迟到的保存结果不写新页面", async () => {
  const editor = useAgentSettings();
  loadWith("p1");
  await editor.load("p1");
  const pending = defer<never>();
  PATCH.mockReturnValueOnce(pending.promise);
  const saving = editor.save("worker", { instructions: "p1 saved" });
  loadWith("p2");
  await editor.load("p2");
  pending.resolve(result(view("p1 saved")));
  expect(await saving).toBe(false);
  expect(editor.views.value[0]?.effective.instructions).toBe("p2");
  expect(editor.saved.value).toBe("");
  const closing = defer<never>();
  PATCH.mockReturnValueOnce(closing.promise);
  const closedSave = editor.save("worker", {});
  editor.dispose();
  closing.resolve(result(view("closed")));
  expect(await closedSave).toBe(false);
  expect(editor.views.value[0]?.effective.instructions).toBe("p2");
});
