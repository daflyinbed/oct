import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { effectScope, nextTick, ref } from "vue";
import client from "@/api/client";
import { useGit } from "./useGit";

vi.mock("@/api/client", () => ({ default: { GET: vi.fn(), POST: vi.fn() } }));
const GET = vi.mocked(client.GET);
const POST = vi.mocked(client.POST);
const scopes: ReturnType<typeof effectScope>[] = [];
function state(branch: string) {
  return {
    root: "/tmp/test",
    branch,
    upstream: null,
    ahead: 0,
    behind: 0,
    remotes: [],
    files: [],
  };
}

beforeEach(() => {
  GET.mockReset();
  POST.mockReset();
  vi.stubGlobal("window", {
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
    setInterval: vi.fn(() => 1),
    clearInterval: vi.fn(),
  });
});
afterEach(() => {
  for (const s of scopes.splice(0)) s.stop();
  vi.unstubAllGlobals();
});
function mount(id = ref<string | null>("p1")) {
  const scope = effectScope();
  scopes.push(scope);
  return { id, git: scope.run(() => useGit(id))! };
}
it("late status from the previous project never replaces the active project", async () => {
  let resolveOld!: (value: unknown) => void;
  const old = new Promise((resolve) => {
    resolveOld = resolve;
  });
  GET.mockImplementation(((
    path: string,
    options: { params: { path: { projectId: string } } },
  ) => {
    if (!path.endsWith("status")) return Promise.resolve({ data: [] });
    return options.params.path.projectId === "p1"
      ? old
      : Promise.resolve({ data: state("new") });
  }) as never);
  const { id, git } = mount();
  id.value = "p2";
  await nextTick();
  await vi.waitFor(() => expect(git.status.value?.branch).toBe("new"));
  resolveOld({ data: state("old") });
  await nextTick();
  await nextTick();
  expect(git.status.value?.branch).toBe("new");
  id.value = null;
  await nextTick();
  expect(git.status.value).toBeNull();
  expect(git.loading.value).toBe(false);
});
it("failed mutation refreshes repository state and retains the actionable error", async () => {
  GET.mockImplementation(((path: string) =>
    Promise.resolve({
      data: path.endsWith("status") ? state("main") : [],
    })) as never);
  POST.mockResolvedValue({
    error: { error: "CONFLICT: resolve tracked.txt" },
  } as never);
  const { git } = mount();
  await vi.waitFor(() => expect(git.status.value?.branch).toBe("main"));
  const before = GET.mock.calls.length;
  await git.run("stash_pop");
  expect(GET.mock.calls.length).toBeGreaterThan(before);
  expect(git.error.value).toBe("CONFLICT: resolve tracked.txt");
  expect(git.busy.value).toBe(false);
});
