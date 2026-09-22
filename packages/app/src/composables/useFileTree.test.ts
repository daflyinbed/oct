import { beforeEach, describe, expect, it, vi } from "vitest";
import client from "@/api/client";
import { useFileTree } from "@/composables/useFileTree";
import type { TreeNode } from "@/composables/useFileTree";

vi.mock("@/api/client", () => ({
  default: { GET: vi.fn() },
}));

const GET = vi.mocked(client.GET);

const { items, expanded, loadRoot } = useFileTree();

function entry(name: string, kind: "dir" | "file", path: string) {
  return { name, path, kind };
}

beforeEach(() => {
  GET.mockReset();
});

// openapi-fetch 的 GET 是按路径的重载集合，vi.mocked 只取其中一个签名，
// 与实际请求的端点对不上——按仓库惯例整体 as never 绕开。
function stubFiles(data: unknown[]) {
  GET.mockResolvedValue({ data, error: undefined } as never);
}

describe("loadRoot（多根工作区）", () => {
  it("附加目录作为顶层根节点排在主目录树之后，path 为绝对路径", async () => {
    stubFiles([entry("src", "dir", "src"), entry("a.txt", "file", "a.txt")]);

    await loadRoot("p1", ["/ext/repo", "  ", "/ext/repo/"]);

    expect(
      items.value.map((n) => [n.name, n.path, n.additionalRoot ?? false]),
    ).toEqual([
      ["src", "src", false],
      ["a.txt", "a.txt", false],
      ["repo", "/ext/repo", true],
      ["repo", "/ext/repo/", true],
    ]);
  });

  it("展开附加根节点时按其绝对路径懒加载子项", async () => {
    stubFiles([]);
    await loadRoot("p1", ["/ext/repo"]);

    stubFiles([entry("README.md", "file", "/ext/repo/README.md")]);
    expanded.value = ["/ext/repo"];

    await vi.waitFor(() => {
      const root = items.value.find(
        (n): n is TreeNode => n.path === "/ext/repo",
      );
      expect(root?.children).toHaveLength(1);
      expect(root?.children?.[0]?.path).toBe("/ext/repo/README.md");
    });
    expect(GET).toHaveBeenLastCalledWith("/api/projects/{projectId}/files", {
      params: { path: { projectId: "p1" }, query: { path: "/ext/repo" } },
    });
  });

  it("不传附加目录时根层只有主目录内容", async () => {
    stubFiles([entry("a.txt", "file", "a.txt")]);

    await loadRoot("p2");

    expect(items.value.map((n) => n.path)).toEqual(["a.txt"]);
  });
});
