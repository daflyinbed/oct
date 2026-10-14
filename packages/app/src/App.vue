<template>
  <div class="workspace-shell">
    <TitleBar
      :projects="projects"
      :active-project-id="activeProjectId"
      @select-project="handleSelectProject"
      @create-project="showNewProject = true"
      @edit-project="editingProject = activeProject ?? null"
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
          @delete-backup="handleDeleteBackup"
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
        <div class="h-full min-h-0 min-w-0 flex flex-col">
          <WorkspaceTabBar
            :tabs="displayTabs"
            @select="activateTab"
            @close="closeTab"
            @tab-action="handleTabAction"
          />
          <div class="min-h-0 flex flex-1 flex-col">
            <!-- Chat 内容走路由（深链/侧栏入口共用）；其余标签类型直接切换渲染 -->
            <div
              v-show="!activeTab || activeTab.kind === 'chat'"
              class="min-h-0 flex flex-1 flex-col"
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
              :key="activeTab.id"
              :title="activeTab.title"
              :project-id="activeTab.projectId ?? null"
              :selection="activeTab.diffSelection"
            />
            <GitGraphTabView v-else-if="activeTab?.kind === 'git-graph'" />
            <McpTabView
              v-else-if="activeTab?.kind === 'mcp'"
              :server-id="activeTab.payload ?? ''"
              :active-project-id="activeProjectId"
            />
            <!-- 子代理标签：每个 agent 一个实例（keyed），v-show 保活——
                 切换标签保留子面板的消息/滚动状态；关闭标签才销毁（只关
                 视图，不取消任务） -->
            <SubagentTabView
              v-for="tab in subagentTabs"
              v-show="activeTab?.kind === 'subagent' && activeTabId === tab.id"
              :key="tab.id"
              :agent-id="tab.payload ?? ''"
              :root-conversation-id="tab.rootConversationId ?? null"
              :description="tab.title ?? ''"
            />
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
          :project-id="activeProjectId"
          @open-diff="handleOpenDiffTab"
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
          :additional-dirs="activeProject?.additional_dirs ?? []"
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
      :active-project-id="activeProjectId"
      :root-conversation-id="selectedConversationId"
      @toggle-sidebar="showSidebar = !showSidebar"
      @toggle-chat="showChat = !showChat"
      @toggle-diff="showDiffPanel = !showDiffPanel"
      @toggle-file-tree="showFileTree = !showFileTree"
      @configure-mcp-server="handleConfigureMcpServer"
    />

    <SettingsModal
      :visible="showSettings"
      :active-project-id="activeProjectId"
      :focus-server-id="mcpFocusServerId"
      @close="closeSettings"
    />
    <!-- 新建与编辑共用一个对话框：project 为 null 时是新建模式。 -->
    <ProjectDialog
      :visible="showNewProject || editingProject !== null"
      :project="editingProject"
      @close="closeProjectDialog"
    />
  </div>
</template>

<script setup lang="ts">
import { SplitterGroup, SplitterPanel, SplitterResizeHandle } from "reka-ui";
import { computed, onMounted, onScopeDispose, ref, watch } from "vue";
import { useRoute, useRouter } from "vue-router";
import BottomBar from "@/components/BottomBar.vue";
import DiffPanel from "@/components/diff/DiffPanel.vue";
import FileTreePanel from "@/components/file-tree/FileTreePanel.vue";
import ProjectDialog from "@/components/layout/ProjectDialog.vue";
import WorkspaceTabBar from "@/components/layout/WorkspaceTabBar.vue";
import SettingsModal from "@/components/settings/SettingsModal.vue";
import ProjectSidebar from "@/components/sidebar/ProjectSidebar.vue";
import SubagentTabView from "@/components/subagent/SubagentTabView.vue";
import DiffTabView from "@/components/views/DiffTabView.vue";
import FileTabView from "@/components/views/FileTabView.vue";
import GitGraphTabView from "@/components/views/GitGraphTabView.vue";
import McpTabView from "@/components/views/McpTabView.vue";
import {
  refreshBackgroundTasks,
  setWakeRunHandler,
} from "@/composables/useBackgroundTasks";
import { onTitleUpdated, useChat } from "@/composables/useChat";
import { useProjects } from "@/composables/useProjects";
import { useProviders } from "@/composables/useProviders";
import { refreshSubagents } from "@/composables/useSubagents";
import { useTabs } from "@/composables/useTabs";
import type { components } from "@/api/schema";
import type { TabAction } from "@/components/layout/WorkspaceTabBar.vue";
import type { DiffSelection } from "@/composables/useGit";

const route = useRoute();
const router = useRouter();

const {
  projects,
  conversations,
  fetchProjects,
  createConversation,
  deleteConversation,
  updateConversationTitle,
} = useProjects();
const { fetchProviders } = useProviders();
const { sending, fetchMessages, attachRun } = useChat();

// 后台标题生成落地 → 会话列表/标签页标题即时跟上（title_updated 是会话
// 元数据事件，useChat 在 SSE 消费层派发；重连重放会重复到达，覆盖即幂等）。
// 随组件作用域销毁退订：生产环境 App 只挂载一次，e2e 里每次挂载新 App 时
// 不残留上一轮的监听器。
onScopeDispose(
  onTitleUpdated((conversationId, title) => {
    updateConversationTitle(conversationId, title);
  }),
);
const {
  tabs,
  activeTab,
  activeTabId,
  openTab,
  closeTab,
  activateTab,
  tabTitle,
  tabOriginKind,
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

const activeProject = computed(() =>
  projects.value.find((project) => project.id === activeProjectId.value),
);

const showSidebar = ref(true);
const showChat = ref(true);
const showDiffPanel = ref(true);
const showFileTree = ref(true);
const showSettings = ref(false);
const showNewProject = ref(false);
/** 非空 = 打开设置时直接定位到该 MCP server 的编辑表单（状态栏菜单深链）。 */
const mcpFocusServerId = ref<string | null>(null);
/** 新建与编辑共用一个对话框：project 为 null 时是新建模式。 */
const editingProject = ref<components["schemas"]["Project"] | null>(null);

function closeSettings() {
  showSettings.value = false;
  // 清空深链目标：此后从顶栏「设置」普通打开时不得再触发定位
  mcpFocusServerId.value = null;
}

function handleConfigureMcpServer(serverId: string) {
  mcpFocusServerId.value = serverId;
  showSettings.value = true;
}

function closeProjectDialog() {
  showNewProject.value = false;
  editingProject.value = null;
}

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
    // 根会话的子任务快照：切会话即拉取（store 独立于可见 panel 存活）。
    // 事件流期间的实时更新由 useChat 单例的 subagent_updated 派发维护。
    void refreshSubagents(id);
    // 后台任务快照同理：切会话对账一次（PR1 无 hub 小事件，运行中任务
    // 由 store 自己的轮询维持刷新，见 useBackgroundTasks）。
    void refreshBackgroundTasks(id);
  },
  { immediate: true },
);

// 后台任务唤醒 run 的可见性（P1）：会话空闲时后端在任务终态后自动起的
// wake run 没有前端 SSE 订阅——store 轮询在结算后的宽限窗内探测 /run，
// 复用 attachRun 对账：running 挂回流，idle 刷新历史以覆盖快速结束的 wake。
// 视图已
// 切走时不挂（切回时上方 tab-return watch 的 fetchMessages + attachRun
// 会兜底对账）。
setWakeRunHandler((conversationId) => {
  if (selectedConversationId.value !== conversationId) return;
  return attachRun(conversationId);
});

/** 打开中的子代理标签（每个 agent 一个实例，App 层渲染并 v-show 保活）。 */
const subagentTabs = computed(() =>
  tabs.value.filter((tab) => tab.kind === "subagent"),
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
      // 流式进行中跳过——live 视图比库里的更完整；随后 attachRun 兜住
      // "刷新瞬间 run 恰好还在后端跑着"的情况（如另一窗口发起的 run）。
      const conversationId = tab.payload;
      void fetchMessages(conversationId)
        .then(() => attachRun(conversationId))
        // 重连对账完成后再拉一次子任务快照（流内事件之外的状态收敛）。
        .then(() => refreshSubagents(conversationId))
        // 后台任务快照同样对账（离开标签期间的任务结算/唤醒 run 落定）。
        .then(() => refreshBackgroundTasks(conversationId));
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
    originKind: tabOriginKind(tab),
  })),
);

function handleSelectConversation(conversationId: string) {
  router.push(`/conversation/${conversationId}`);
}

async function handleDeleteBackup(projectId: string, id: string) {
  if (
    conversations.value.get(projectId)?.find((c) => c.id === id)
      ?.origin_kind !== "revert_backup"
  )
    return;
  await deleteConversation(projectId, id);
  if (!conversations.value.get(projectId)?.some((c) => c.id === id))
    closeTab(`chat:${id}`);
}

function handleTabAction(action: TabAction) {
  if (action === "add-chat") void handleAddChatTab();
  else if (action === "open-diff") handleOpenDiffTab();
  else handleOpenGraphTab();
}

async function handleAddChatTab() {
  const projectId = activeProjectId.value ?? projects.value[0]?.id;
  if (!projectId) {
    showNewProject.value = true;
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

function handleOpenDiffTab(selection: DiffSelection = {}) {
  if (!activeProjectId.value) return;
  showChat.value = true;
  const title = selection.revision
    ? `${selection.stash ? "Stash" : "Commit"} ${selection.revision.slice(0, 8)}`
    : (selection.path ?? "Working Tree");
  openTab({
    id: `diff:${activeProjectId.value}:${JSON.stringify(selection)}`,
    kind: "diff",
    title,
    projectId: activeProjectId.value,
    diffSelection: selection,
  });
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

async function handleCreateConversation(projectId: string) {
  const conv = await createConversation(projectId);
  if (conv) {
    router.push(`/conversation/${conv.id}`);
  }
}

onMounted(() => {
  fetchProjects();
  fetchProviders();
});
</script>
