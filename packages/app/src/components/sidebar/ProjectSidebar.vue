<template>
  <aside
    class="flex-shrink-0 w-[260px] flex flex-col border-r"
    style="
      border-color: var(--color-border);
      background-color: var(--color-panel);
    "
  >
    <div class="p-4 border-b" style="border-color: var(--color-border)">
      <div class="flex items-center justify-between">
        <span class="text-[1rem] font-semibold" style="color: var(--color-text)"
          >Projects</span
        >
        <button
          class="px-2 py-1 text-[0.875rem] font-medium"
          style="color: var(--color-accent-9); border-radius: var(--radius-md)"
          @click="$emit('create-project')"
          @mouseenter="
            (e: MouseEvent) =>
              ((e.currentTarget as HTMLElement).style.backgroundColor =
                'var(--color-surface)')
          "
          @mouseleave="
            (e: MouseEvent) =>
              ((e.currentTarget as HTMLElement).style.backgroundColor =
                'transparent')
          "
        >
          + New
        </button>
      </div>
    </div>

    <div class="flex-1 overflow-y-auto p-2">
      <div v-for="project in projects" :key="project.id" class="mb-2">
        <button
          class="w-full flex items-center gap-2 px-3 py-2 text-left text-[1rem] font-medium transition-colors"
          style="border-radius: var(--radius-md); color: var(--color-text)"
          :style="{
            backgroundColor: expandedProjects.has(project.id)
              ? 'var(--color-surface)'
              : 'transparent',
          }"
          @click="toggleProject(project.id)"
          @mouseenter="
            (e: MouseEvent) => {
              if (!expandedProjects.has(project.id))
                (e.currentTarget as HTMLElement).style.backgroundColor =
                  'var(--color-surface)';
            }
          "
          @mouseleave="
            (e: MouseEvent) => {
              if (!expandedProjects.has(project.id))
                (e.currentTarget as HTMLElement).style.backgroundColor =
                  'transparent';
            }
          "
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
            class="w-full flex flex-col items-start px-3 py-2 text-left text-[0.9rem] transition-colors"
            style="border-radius: var(--radius-md); color: var(--color-text)"
            :style="{
              backgroundColor:
                selectedConversationId === conv.id
                  ? 'var(--color-surface)'
                  : 'transparent',
            }"
            @click="$emit('select-conversation', conv.id, project.id)"
            @mouseenter="
              (e: MouseEvent) => {
                if (selectedConversationId !== conv.id)
                  (e.currentTarget as HTMLElement).style.backgroundColor =
                    'var(--color-surface)';
              }
            "
            @mouseleave="
              (e: MouseEvent) => {
                if (selectedConversationId !== conv.id)
                  (e.currentTarget as HTMLElement).style.backgroundColor =
                    'transparent';
              }
            "
          >
            <span class="truncate w-full" style="color: var(--color-text)">{{
              conv.title || "New Chat"
            }}</span>
          </button>

          <button
            class="w-full flex items-center gap-2 px-3 py-1.5 text-left text-[0.875rem] transition-colors"
            style="border-radius: var(--radius-md); color: var(--color-muted)"
            @click="$emit('create-conversation', project.id)"
            @mouseenter="
              (e: MouseEvent) =>
                ((e.currentTarget as HTMLElement).style.backgroundColor =
                  'var(--color-surface)')
            "
            @mouseleave="
              (e: MouseEvent) =>
                ((e.currentTarget as HTMLElement).style.backgroundColor =
                  'transparent')
            "
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
