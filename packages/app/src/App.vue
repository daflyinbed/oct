<template>
  <div class="workspace-shell">
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
          @create-project="handleCreateProject"
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
        <RouterView />
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
        />
      </SplitterPanel>
    </SplitterGroup>

    <div
      v-else
      class="workspace-panels items-center justify-center bg-background"
    >
      <p class="text-neutral-10/60">
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
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { useRoute, useRouter } from "vue-router";
import { SplitterGroup, SplitterPanel, SplitterResizeHandle } from "reka-ui";
import BottomBar from "@/components/BottomBar.vue";
import DiffPanel from "@/components/diff/DiffPanel.vue";
import FileTreePanel from "@/components/file-tree/FileTreePanel.vue";
import ProjectSidebar from "@/components/sidebar/ProjectSidebar.vue";
import { useProjects } from "@/composables/useProjects";

const route = useRoute();
const router = useRouter();

const {
  projects,
  conversations,
  fetchProjects,
  createProject,
  createConversation,
} = useProjects();

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

function handleSelectConversation(conversationId: string, _projectId: string) {
  router.push(`/conversation/${conversationId}`);
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
});
</script>
