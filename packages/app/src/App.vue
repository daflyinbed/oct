<template>
  <div class="h-full flex flex-col overflow-hidden">
    <div class="flex-1 flex overflow-hidden">
      <ProjectSidebar
        v-if="showSidebar"
        :projects="projects"
        :conversations="conversations"
        :selected-conversation-id="selectedConversationId"
        @select-conversation="handleSelectConversation"
        @create-project="handleCreateProject"
        @create-conversation="handleCreateConversation"
      />

      <RouterView v-if="showChat" />

      <template v-if="!showSidebar && !showChat">
        <div
          class="flex-1 flex items-center justify-center"
          style="background-color: var(--color-bg)"
        >
          <p style="color: var(--color-muted)">
            All panels are hidden. Use the bottom bar to show them.
          </p>
        </div>
      </template>

      <DiffPanel
        v-if="showDiffPanel"
        :visible="showDiffPanel"
        :diff-files="diffFiles"
        @close="showDiffPanel = false"
      />

      <FileTreePanel
        v-if="showFileTree"
        :visible="showFileTree"
        :files="fileTree"
        @close="showFileTree = false"
        @toggle-folder="handleToggleFolder"
      />
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

const showSidebar = ref(true);
const showChat = ref(true);
const showDiffPanel = ref(true);
const showFileTree = ref(true);

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

function handleToggleFolder(path: string) {
  console.log("toggle folder", path);
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

const fileTree = ref([
  { type: "folder" as const, name: "crates", path: "crates" },
  {
    type: "file" as const,
    name: "Cargo.toml",
    path: "Cargo.toml",
    status: "unchanged",
  },
  {
    type: "file" as const,
    name: "README.md",
    path: "README.md",
    status: "modified",
  },
  { type: "folder" as const, name: "migrations", path: "migrations" },
  {
    type: "file" as const,
    name: ".gitignore",
    path: ".gitignore",
    status: "unchanged",
  },
  {
    type: "file" as const,
    name: "package.json",
    path: "package.json",
    status: "added",
  },
  {
    type: "file" as const,
    name: "old-config.yml",
    path: "old-config.yml",
    status: "deleted",
  },
]);

onMounted(() => {
  fetchProjects();
});
</script>
