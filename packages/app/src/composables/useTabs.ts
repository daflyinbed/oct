import { computed, readonly, ref } from "vue";
import { useProjects } from "./useProjects";

export type TabKind = "chat" | "file" | "diff" | "git-graph";

export interface WorkspaceTab {
  /** 稳定唯一键：`chat:<conversationId>`、`file:<projectId>:<path>`、`diff:<key>`… */
  id: string;
  kind: TabKind;
  /** chat → conversation id；file → 文件路径 */
  payload?: string;
  /** 静态标题兜底；chat 的标题始终从会话数据实时解析 */
  title?: string;
  projectId?: string | null;
}

const tabs = ref<WorkspaceTab[]>([]);
const activeTabId = ref<string | null>(null);

export function useTabs() {
  const { conversations } = useProjects();

  const activeTab = computed(
    () => tabs.value.find((tab) => tab.id === activeTabId.value) ?? null,
  );

  function openTab(tab: WorkspaceTab) {
    if (!tabs.value.some((existing) => existing.id === tab.id)) {
      tabs.value = [...tabs.value, tab];
    }
    activeTabId.value = tab.id;
  }

  function closeTab(id: string) {
    const index = tabs.value.findIndex((tab) => tab.id === id);
    if (index === -1) return;
    tabs.value = tabs.value.filter((tab) => tab.id !== id);
    if (activeTabId.value === id) {
      const next = tabs.value[Math.min(index, tabs.value.length - 1)];
      activeTabId.value = next?.id ?? null;
    }
  }

  function activateTab(id: string) {
    if (tabs.value.some((tab) => tab.id === id)) {
      activeTabId.value = id;
    }
  }

  function tabTitle(tab: WorkspaceTab): string {
    if (tab.kind === "chat") {
      for (const list of conversations.value.values()) {
        const conversation = list.find((item) => item.id === tab.payload);
        if (conversation) return conversation.title || "New Chat";
      }
      return "New Chat";
    }
    if (tab.kind === "file") {
      const segments = (tab.payload ?? "").split("/");
      return segments.at(-1) || "File";
    }
    return tab.title ?? tab.kind;
  }

  return {
    tabs: readonly(tabs),
    activeTabId: readonly(activeTabId),
    activeTab: readonly(activeTab),
    openTab,
    closeTab,
    activateTab,
    tabTitle,
  };
}
