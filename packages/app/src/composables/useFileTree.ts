import { readonly, ref, watch } from "vue";
import client from "@/api/client";
import type { components } from "@/api/schema";

type FileEntry = components["schemas"]["FileEntry"];

export interface TreeNode {
  name: string;
  path: string;
  kind: "dir" | "file";
  /** Directories carry an (initially empty) array so reka-ui allows toggling
   * them before their children are lazily fetched; files stay undefined. */
  children?: TreeNode[];
}

const items = ref<TreeNode[]>([]);
const expanded = ref<string[]>([]);
const loading = ref(false);
const error = ref<string | null>(null);

let currentProjectId: string | null = null;
const loadedDirs = new Set<string>();
const pendingDirs = new Set<string>();
// Bumps on every project switch so stale responses get dropped.
let loadSeq = 0;

function entriesToNodes(entries: FileEntry[]): TreeNode[] {
  return entries.map((entry) => ({
    name: entry.name,
    path: entry.path,
    kind: entry.kind,
    children: entry.kind === "dir" ? [] : undefined,
  }));
}

function toErrorMessage(e: unknown, fallback: string): string {
  return e instanceof Error && e.message ? e.message : fallback;
}

async function fetchEntries(projectId: string, path: string): Promise<TreeNode[]> {
  const { data, error: err } = await client.GET(
    "/api/projects/{projectId}/files",
    {
      params: {
        path: { projectId },
        query: path ? { path } : undefined,
      },
    },
  );
  if (err || !data) {
    throw new Error(
      (err as unknown as { error?: string } | undefined)?.error ??
        "Failed to load directory",
    );
  }
  return entriesToNodes(data);
}

function findNode(path: string): TreeNode | null {
  const queue: TreeNode[] = [...items.value];
  while (queue.length > 0) {
    const node = queue.shift();
    if (!node) break;
    if (node.path === path) return node;
    if (node.children) queue.push(...node.children);
  }
  return null;
}

async function loadRoot(projectId: string) {
  const seq = ++loadSeq;
  currentProjectId = projectId;
  loading.value = true;
  error.value = null;
  items.value = [];
  expanded.value = [];
  loadedDirs.clear();
  pendingDirs.clear();

  try {
    const nodes = await fetchEntries(projectId, "");
    if (seq !== loadSeq) return;
    items.value = nodes;
    loadedDirs.add("");
  } catch (e) {
    if (seq === loadSeq) error.value = toErrorMessage(e, "Failed to load files");
  } finally {
    if (seq === loadSeq) loading.value = false;
  }
}

async function ensureChildren(projectId: string, node: TreeNode) {
  if (node.kind !== "dir" || loadedDirs.has(node.path)) return;
  if (pendingDirs.has(node.path)) return;
  pendingDirs.add(node.path);
  try {
    const children = await fetchEntries(projectId, node.path);
    if (projectId !== currentProjectId) return;
    node.children = children;
    loadedDirs.add(node.path);
  } catch (e) {
    if (projectId === currentProjectId) {
      error.value = toErrorMessage(e, `Failed to load ${node.path}`);
    }
  } finally {
    pendingDirs.delete(node.path);
  }
}

// Lazily fetch children of newly expanded directories.
watch(expanded, (keys) => {
  const projectId = currentProjectId;
  if (!projectId) return;
  for (const key of keys) {
    const node = findNode(key);
    if (node) void ensureChildren(projectId, node);
  }
});

export function useFileTree() {
  return {
    // Raw ref (not readonly) so the TreeRoot generic infers TreeNode cleanly.
    items,
    expanded,
    loading: readonly(loading),
    error: readonly(error),
    loadRoot,
  };
}
