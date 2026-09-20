<template>
  <aside class="workspace-panel h-full w-full bg-panel">
    <header class="workspace-panel-header flex-none">
      <span
        class="text-[11px] text-dim font-semibold tracking-[0.8px] uppercase"
        >Threads</span
      >
    </header>

    <!-- 搜索 -->
    <div class="flex-none px-2.5 pb-2.5 pt-0.5">
      <div
        class="h-7 flex items-center gap-[7px] border border-line-soft rounded-md bg-surface px-[9px] text-faint transition-colors focus-within:border-accent-6"
      >
        <i-lucide-search class="h-3 w-3 flex-none" />
        <input
          v-model="filter"
          placeholder="Search threads…"
          class="min-w-0 flex-1 border-none bg-transparent text-[12.5px] text-neutral-10 outline-none placeholder:text-faint"
        />
      </div>
    </div>

    <div class="workspace-panel-body px-1.5 pb-3">
      <div v-for="project in visibleProjects" :key="project.id" class="mb-1">
        <button
          class="group/pg h-7 w-full flex items-center gap-1.5 rounded-md px-2 text-left text-[12.5px] text-neutral-10 font-semibold control-focus transition-colors hover:bg-neutral-1"
          @click="toggleProject(project.id)"
        >
          <i-lucide-chevron-right
            class="h-2.5 w-2.5 flex-none text-faint transition-transform"
            :class="expandedProjects.has(project.id) ? 'rotate-90' : ''"
          />
          <span class="truncate">{{ project.name }}</span>
          <span
            class="flex-none rounded-full bg-neutral-2 px-1.5 text-[10.5px] text-faint font-normal leading-[16px]"
          >
            {{ (conversations.get(project.id) ?? []).length }}
          </span>
          <span class="flex-1" />
          <span
            class="h-5 w-5 inline-flex flex-none items-center justify-center rounded-[4px] text-dim opacity-0 transition-[opacity,colors] hover:bg-neutral-2 hover:text-neutral-10 group-hover/pg:opacity-100"
            title="在此项目新建对话"
            @click.stop="$emit('createConversation', project.id)"
          >
            <i-lucide-plus class="h-[11px] w-[11px]" />
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
            class="relative w-full flex items-center rounded-md py-[5px] pl-[26px] pr-2 text-left text-[12.5px] control-focus transition-colors"
            :class="
              selectedConversationId === conv.id
                ? 'bg-accent-3 text-neutral-10'
                : 'text-dim hover:bg-neutral-1 hover:text-neutral-10'
            "
            @click="$emit('selectConversation', conv.id, project.id)"
          >
            <span
              v-if="selectedConversationId === conv.id"
              class="absolute left-2 top-1/2 h-3.5 w-[3px] rounded-full bg-accent-7 -translate-y-1/2"
            />
            <span class="truncate">{{ conv.title || "New Chat" }}</span>
          </button>

          <button
            class="w-full flex items-center gap-1.5 rounded-md py-[5px] pl-[26px] pr-2 text-left text-[12.5px] text-faint control-focus transition-colors hover:bg-neutral-1 hover:text-accent-10"
            @click="$emit('createConversation', project.id)"
          >
            <i-lucide-plus class="h-3 w-3" />
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
  selectConversation: [conversationId: string, projectId: string];
  createConversation: [projectId: string];
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
