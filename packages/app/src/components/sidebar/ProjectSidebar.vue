<template>
  <aside class="workspace-panel h-full w-full bg-panel">
    <header
      class="workspace-panel-header justify-between border-b border-neutral-5"
    >
      <span class="text-[1rem] font-semibold text-neutral-10">Projects</span>
      <button
        class="control-ghost control-focus px-2 py-1 text-[0.875rem] font-medium text-accent-10"
        @click="$emit('create-project')"
      >
        + New
      </button>
    </header>

    <div class="workspace-panel-body p-2">
      <div v-for="project in projects" :key="project.id" class="mb-2">
        <button
          class="control-ghost control-focus w-full flex items-center gap-2 px-3 py-2 text-left text-[1rem] font-medium transition-colors"
          :class="
            expandedProjects.has(project.id)
              ? 'bg-neutral-2 text-neutral-10'
              : ''
          "
          @click="toggleProject(project.id)"
        >
          <span
            class="text-[0.875rem] transition-transform"
            :class="expandedProjects.has(project.id) ? 'rotate-90' : ''"
            >▶</span
          >
          <span class="truncate">{{ project.name }}</span>
        </button>

        <div
          v-if="expandedProjects.has(project.id)"
          class="mt-1 ml-4 space-y-0.5"
        >
          <button
            v-for="conv in conversations.get(project.id) ?? []"
            :key="conv.id"
            class="control-ghost control-focus w-full flex flex-col items-start px-3 py-2 text-left text-[0.9rem] transition-colors"
            :class="
              selectedConversationId === conv.id
                ? 'bg-neutral-2 text-neutral-10'
                : ''
            "
            @click="$emit('select-conversation', conv.id, project.id)"
          >
            <span class="truncate w-full text-neutral-10">{{
              conv.title || "New Chat"
            }}</span>
          </button>

          <button
            class="control-ghost control-focus w-full flex items-center gap-2 px-3 py-1.5 text-left text-[0.875rem] transition-colors"
            @click="$emit('create-conversation', project.id)"
          >
            + New Chat
          </button>
        </div>
      </div>
    </div>
  </aside>
</template>

<script setup lang="ts">
import { ref } from "vue";
import type { components } from "@/api/schema";

type Project = components["schemas"]["Project"];
type Conversation = components["schemas"]["Conversation"];

const props = defineProps<{
  projects: readonly Project[];
  conversations: ReadonlyMap<string, readonly Conversation[]>;
  selectedConversationId: string | null;
}>();

defineEmits<{
  "select-conversation": [conversationId: string, projectId: string];
  "create-project": [];
  "create-conversation": [projectId: string];
}>();

const expandedProjects = ref<Set<string>>(new Set());

function toggleProject(id: string) {
  const next = new Set(expandedProjects.value);
  if (next.has(id)) next.delete(id);
  else next.add(id);
  expandedProjects.value = next;
}
</script>
