<template>
  <div class="workspace-shell">
    <TitleBar
      :projects="projects"
      :active-project-id="activeProjectId"
      @select-project="handleSelectProject"
      @create-project="handleCreateProject"
      @open-settings="showSettings = true"
    />

    <SplitterGroup
      v-if="hasVisiblePanel"
      class="workspace-panels"
      direction="horizontal"
      auto-save-id="oct-workspace-layout"
    >
      <SplitterPanel
        v-if="showSidebar"
        id="project-sidebar-panel"
        :order="1"
        :default-size="20"
        :min-size="12"
        :max-size="33"
      >
        <ProjectSidebar
          :projects="projects"
          :conversations="conversations"
          :selected-conversation-id="selectedConversationId"
          @select-conversation="handleSelectConversation"
          @create-conversation="handleCreateConversation"
        />
      </SplitterPanel>
      <SplitterResizeHandle
        v-if="showSidebar && hasPanelAfterSidebar"
        id="project-sidebar-resize-handle"
        class="workspace-resize-handle"
      />

      <SplitterPanel
        v-if="showChat"
        id="chat-panel"
        :order="2"
        :default-size="40"
        :min-size="25"
      >
        <!-- h-full 而非 flex-1：SplitterPanel 的 DOM 不是 flex 容器，
             百分比高度才是让内部 flex-1/overflow 链生效的锚点 -->
        <div class="flex flex-col h-full min-h-0 min-w-0">
          <WorkspaceTabBar
            :tabs="displayTabs"
            @select="activateTab"
            @close="closeTab"
            @tab-action="handleTabAction"
          />
          <div class="flex flex-col flex-1 min-h-0">
            <!-- Chat 内容走路由（深链/侧栏入口共用）；其余标签类型直接切换渲染 -->
            <div
              v-show="!activeTab || activeTab.kind === 'chat'"
              class="flex flex-col flex-1 min-h-0"
            >
              <RouterView />
            </div>
            <FileTabView
              v-if="activeTab?.kind === 'file'"
              :key="activeTab.id"
              :path="activeTab.payload ?? ''"
              :project-id="activeTab.projectId ?? null"
            />
            <DiffTabView
              v-else-if="activeTab?.kind === 'diff'"
              title="Working Tree"
              :diff-files="diffFiles"
            />
            <GitGraphTabView v-else-if="activeTab?.kind === 'git-graph'" />
          </div>
        </div>
      </SplitterPanel>
      <SplitterResizeHandle
        v-if="showChat && hasPanelAfterChat"
        id="chat-resize-handle"
        class="workspace-resize-handle"
      />

      <SplitterPanel
        v-if="showDiffPanel"
        id="diff-panel"
        :order="3"
        :default-size="25"
        :min-size="15"
        :max-size="55"
      >
        <DiffPanel
          :visible="showDiffPanel"
          :diff-files="diffFiles"
          @close="showDiffPanel = false"
        />
      </SplitterPanel>
      <SplitterResizeHandle
        v-if="showDiffPanel && showFileTree"
        id="diff-resize-handle"
        class="workspace-resize-handle"
      />

      <SplitterPanel
        v-if="showFileTree"
        id="file-tree-panel"
        :order="4"
        :default-size="15"
        :min-size="12"
        :max-size="35"
      >
        <FileTreePanel
          :visible="showFileTree"
          :project-id="activeProjectId"
          @close="showFileTree = false"
          @open-file="handleOpenFile"
        />
      </SplitterPanel>
    </SplitterGroup>

    <div
      v-else
      class="workspace-panels items-center justify-center bg-background"
    >
      <p class="text-neutral-7">
        All panels are hidden. Use the bottom bar to show them.
      </p>
    </div>

    <BottomBar
      :sidebar-visible="showSidebar"
      :chat-visible="showChat"
      :diff-visible="showDiffPanel"
      :file-tree-visible="showFileTree"
      @toggle-sidebar="showSidebar = !showSidebar"
      @toggle-chat="showChat = !showChat"
      @toggle-diff="showDiffPanel = !showDiffPanel"
      @toggle-file-tree="showFileTree = !showFileTree"
    />

    <SettingsModal :visible="showSettings" @close="showSettings = false" />
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, ref, watch } from "vue";
import { useRoute, useRouter } from "vue-router";
import { SplitterGroup, SplitterPanel, SplitterResizeHandle } from "reka-ui";
import BottomBar from "@/components/BottomBar.vue";
import WorkspaceTabBar, {
  type TabAction,
} from "@/components/layout/WorkspaceTabBar.vue";
import DiffPanel from "@/components/diff/DiffPanel.vue";
import FileTabView from "@/components/views/FileTabView.vue";
import DiffTabView from "@/components/views/DiffTabView.vue";
import GitGraphTabView from "@/components/views/GitGraphTabView.vue";
import FileTreePanel from "@/components/file-tree/FileTreePanel.vue";
import ProjectSidebar from "@/components/sidebar/ProjectSidebar.vue";
import SettingsModal from "@/components/settings/SettingsModal.vue";
import { useChat } from "@/composables/useChat";
import { useProjects } from "@/composables/useProjects";
import { useTabs } from "@/composables/useTabs";
import { useProviders } from "@/composables/useProviders";

const route = useRoute();
const router = useRouter();

const {
  projects,
  conversations,
  fetchProjects,
  createProject,
  createConversation,
} = useProjects();
const { fetchProviders } = useProviders();
const { sending, fetchMessages } = useChat();
const {
  tabs,
  activeTab,
  activeTabId,
  openTab,
  closeTab,
  activateTab,
  tabTitle,
} = useTabs();

const selectedConversationId = computed(() => {
  const id = (route.params as { id?: string }).id;
  return id ?? null;
});

const activeProjectId = computed(() => {
  const conversationId = selectedConversationId.value;
  if (!conversationId) return null;
  for (const [projectId, list] of conversations.value) {
    if (list.some((conversation) => conversation.id === conversationId)) {
      return projectId;
    }
  }
  return null;
});

const showSidebar = ref(true);
const showChat = ref(true);
const showDiffPanel = ref(true);
const showFileTree = ref(true);
const showSettings = ref(false);

const hasVisiblePanel = computed(
  () =>
    showSidebar.value ||
    showChat.value ||
    showDiffPanel.value ||
    showFileTree.value,
);
const hasPanelAfterSidebar = computed(
  () => showChat.value || showDiffPanel.value || showFileTree.value,
);
const hasPanelAfterChat = computed(
  () => showDiffPanel.value || showFileTree.value,
);

/* ---------- 标签页：路由 ↔ chat 标签双向同步，其余类型仅存在于标签中 ---------- */
watch(
  selectedConversationId,
  (id) => {
    if (id) openTab({ id: `chat:${id}`, kind: "chat", payload: id });
  },
  { immediate: true },
);

watch(activeTabId, (id, prevId) => {
  const tab = tabs.value.find((item) => item.id === id);
  if (tab?.kind === "chat" && tab.payload) {
    const target = `/conversation/${tab.payload}`;
    // 前一个激活标签也是 chat 时路由已经变化，[id].vue 的 route watch
    // 会拉取消息，这里无需重复请求（刚关闭的标签不在列表，按非 chat 算）
    const prevWasChat =
      prevId != null &&
      tabs.value.some((item) => item.id === prevId && item.kind === "chat");
    if (route.path !== target) {
      router.push(target);
    } else if (prevId != null && !prevWasChat && !sending.value) {
      // 从非 chat 标签切回本会话时路由不变（chat 区只是 v-show 隐藏），
      // route watch 不会触发：主动刷新，避免残留切换前的旧 live DOM。
      // 流式进行中跳过——live 视图比库里的更完整。
      fetchMessages(tab.payload);
    }
  } else if (!tab && route.path !== "/") {
    router.push("/");
  }
});

const displayTabs = computed(() =>
  tabs.value.map((tab) => ({
    id: tab.id,
    kind: tab.kind,
    title: tabTitle(tab),
    active: tab.id === activeTabId.value,
  })),
);

function handleSelectConversation(conversationId: string) {
  router.push(`/conversation/${conversationId}`);
}

function handleTabAction(action: TabAction) {
  if (action === "add-chat") void handleAddChatTab();
  else if (action === "open-diff") handleOpenDiffTab();
  else handleOpenGraphTab();
}

async function handleAddChatTab() {
  const projectId = activeProjectId.value ?? projects.value[0]?.id;
  if (!projectId) {
    await handleCreateProject();
    return;
  }
  const conversation = await createConversation(projectId);
  if (conversation) {
    router.push(`/conversation/${conversation.id}`);
  }
}

function handleOpenFile(path: string) {
  openTab({
    id: `file:${activeProjectId.value}:${path}`,
    kind: "file",
    payload: path,
    projectId: activeProjectId.value,
  });
}

function handleOpenDiffTab() {
  openTab({ id: "diff:working-tree", kind: "diff", title: "Working Tree" });
}

function handleOpenGraphTab() {
  openTab({ id: "git-graph", kind: "git-graph", title: "Git Graph" });
}

async function handleSelectProject(projectId: string) {
  const latest = conversations.value.get(projectId)?.at(-1);
  if (latest) {
    router.push(`/conversation/${latest.id}`);
    return;
  }
  const conversation = await createConversation(projectId);
  if (conversation) {
    router.push(`/conversation/${conversation.id}`);
  }
}

async function handleCreateProject() {
  const name = prompt("Project name:");
  if (!name) return;
  const workingDir = prompt("Working directory:");
  if (!workingDir) return;
  await createProject(name, workingDir);
}

async function handleCreateConversation(projectId: string) {
  const conv = await createConversation(projectId);
  if (conv) {
    router.push(`/conversation/${conv.id}`);
  }
}

const diffFiles = ref([
  {
    path: "crates/oct-llm-provider/src/adapter/openai_compatible.rs",
    lines: [
      {
        num: 1,
        type: "context",
        content: "use crate::types::{ChatRequest, ChatResponse};",
      },
      { num: 2, type: "remove", content: "use crate::openai::LegacyRequest;" },
      { num: 3, type: "add", content: "use crate::types::UnifiedRequest;" },
      { num: 4, type: "context", content: "" },
      { num: 5, type: "context", content: "pub struct OpenAIAdapter;" },
    ],
  },
  {
    path: "crates/oct-llm-provider/src/types.rs",
    lines: [
      { num: 1, type: "add", content: "pub struct UnifiedRequest {" },
      { num: 2, type: "add", content: "    pub model: String," },
      { num: 3, type: "add", content: "    pub messages: Vec<Message>," },
      { num: 4, type: "add", content: "}" },
    ],
  },
]);

onMounted(() => {
  fetchProjects();
  fetchProviders();
});
</script>
