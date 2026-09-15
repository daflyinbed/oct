<template>
  <aside class="workspace-panel h-full w-full bg-panel">
    <header class="workspace-panel-header flex-none">
      <span
        class="text-[11px] font-semibold tracking-[0.8px] uppercase text-dim"
        >Threads</span
      >
    </header>

    <!-- 搜索 -->
    <div class="flex-none px-2.5 pt-0.5 pb-2.5">
      <div
        class="flex items-center gap-[7px] h-7 px-[9px] rounded-md border border-line-soft bg-surface text-faint focus-within:border-accent-6 transition-colors"
      >
        <i-lucide-search class="w-3 h-3 flex-none" />
        <input
          v-model="filter"
          placeholder="Search threads…"
          class="flex-1 min-w-0 bg-transparent border-none outline-none text-[12.5px] text-neutral-10 placeholder:text-faint"
        />
      </div>
    </div>

    <div class="workspace-panel-body px-1.5 pb-3">
      <div v-for="project in visibleProjects" :key="project.id" class="mb-1">
        <button
          class="group/pg control-focus w-full flex items-center gap-1.5 h-7 px-2 rounded-md text-left text-[12.5px] font-semibold text-neutral-10 hover:bg-neutral-1 transition-colors"
          @click="toggleProject(project.id)"
        >
          <i-lucide-chevron-right
            class="w-2.5 h-2.5 flex-none text-faint transition-transform"
            :class="expandedProjects.has(project.id) ? 'rotate-90' : ''"
          />
          <span class="truncate">{{ project.name }}</span>
          <span
            class="flex-none text-[10.5px] leading-[16px] font-normal text-faint bg-neutral-2 rounded-full px-1.5"
          >
            {{ (conversations.get(project.id) ?? []).length }}
          </span>
          <span class="flex-1" />
          <span
            class="flex-none w-5 h-5 rounded-[4px] inline-flex items-center justify-center text-dim opacity-0 group-hover/pg:opacity-100 hover:bg-neutral-2 hover:text-neutral-10 transition-[opacity,colors]"
            title="在此项目新建对话"
            @click.stop="$emit('create-conversation', project.id)"
          >
            <i-lucide-plus class="w-[11px] h-[11px]" />
          </span>
        </button>

        <div v-if="expandedProjects.has(project.id)" class="space-y-px">
          <button
            v-for="conv in conversations.get(project.id) ?? []"
            v-show="
              !filter.trim() ||
              (conv.title || 'New Chat')
                .toLowerCase()
                .includes(filter.trim().toLowerCase())
            "
            :key="conv.id"
            class="control-focus relative w-full flex items-center rounded-md pl-[26px] pr-2 py-[5px] text-left text-[12.5px] transition-colors"
            :class="
              selectedConversationId === conv.id
                ? 'bg-accent-3 text-neutral-10'
                : 'text-dim hover:bg-neutral-1 hover:text-neutral-10'
            "
            @click="$emit('select-conversation', conv.id, project.id)"
          >
            <span
              v-if="selectedConversationId === conv.id"
              class="absolute left-2 top-1/2 -translate-y-1/2 h-3.5 w-[3px] rounded-full bg-accent-7"
            />
            <span class="truncate">{{ conv.title || "New Chat" }}</span>
          </button>

          <button
            class="control-focus w-full flex items-center gap-1.5 rounded-md pl-[26px] pr-2 py-[5px] text-left text-[12.5px] text-faint hover:bg-neutral-1 hover:text-accent-10 transition-colors"
            @click="$emit('create-conversation', project.id)"
          >
            <i-lucide-plus class="w-3 h-3" />
            New Chat
          </button>
        </div>
      </div>
    </div>
  </aside>
</template>

<script setup lang="ts">
import { computed, ref, watch } from "vue";
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
  "create-conversation": [projectId: string];
}>();

const expandedProjects = ref<Set<string>>(new Set());
const filter = ref("");

// 搜索时隐藏没有匹配对话的项目分组
const visibleProjects = computed(() => {
  const query = filter.value.trim().toLowerCase();
  if (!query) return props.projects;
  return props.projects.filter((project) =>
    (props.conversations.get(project.id) ?? []).some((conversation) =>
      (conversation.title || "New Chat").toLowerCase().includes(query),
    ),
  );
});

function toggleProject(id: string) {
  const next = new Set(expandedProjects.value);
  if (next.has(id)) next.delete(id);
  else next.add(id);
  expandedProjects.value = next;
}

// 选中某个对话时，自动展开它所在的项目分组
watch(
  () => props.selectedConversationId,
  (id) => {
    if (!id) return;
    for (const [projectId, list] of props.conversations) {
      if (list.some((conversation) => conversation.id === id)) {
        if (!expandedProjects.value.has(projectId)) {
          expandedProjects.value = new Set([
            ...expandedProjects.value,
            projectId,
          ]);
        }
        return;
      }
    }
  },
  { immediate: true },
);
</script>
