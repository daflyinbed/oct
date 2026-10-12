<template>
  <aside class="workspace-panel h-full w-full bg-panel">
    <header class="workspace-panel-header flex-none">
      <span
        class="text-[11px] text-dim font-semibold tracking-[0.8px] uppercase"
        >Threads</span
      >
      <span class="flex-1" />
      <!-- 排序切换：按项目分组 / 按最后更新时间分组（redesign demo 的 sortBtn） -->
      <button
        class="h-[22px] w-[22px] inline-flex flex-none items-center justify-center rounded-md transition-colors"
        :class="
          sortByTime
            ? 'bg-accent-3 text-accent-10'
            : 'text-dim hover:bg-neutral-1 hover:text-neutral-10'
        "
        :title="
          sortByTime ? '切换排序：按时间 / 按项目' : '切换排序：按项目 / 按时间'
        "
        @click="sortByTime = !sortByTime"
      >
        <i-lucide-arrow-down-wide-narrow class="h-3 w-3" />
      </button>
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

    <!-- 按时间排序视图：日期分组（今天/昨天/星期X/月份），行 = 标题 + 项目两行 -->
    <div v-if="sortByTime" class="workspace-panel-body px-1.5 pb-3">
      <div v-for="group in timeGroups" :key="group.label">
        <div class="px-2.5 pb-[5px] pt-3 text-[11.5px] text-faint">
          {{ group.label }}
        </div>
        <div class="space-y-px">
          <ThreadItem
            v-for="node in group.nodes"
            :key="node.conv.id"
            :node="node"
            :selected-id="selectedConversationId"
            layout="time"
            :project-name="projectNames.get(node.conv.project_id) ?? ''"
            @select="handleSelect"
            @rename="renaming = $event"
            @delete-backup="handleDeleteBackup"
          />
        </div>
      </div>
    </div>

    <!-- 按项目分组视图 -->
    <div v-else class="workspace-panel-body px-1.5 pb-3">
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
          <!-- 搜索时平铺命中行（不嵌套），否则按 fork/回退备份嵌套成线程树 -->
          <ThreadItem
            v-for="node in threadsOf(project.id)"
            :key="node.conv.id"
            :node="node"
            :selected-id="selectedConversationId"
            @select="(id) => $emit('selectConversation', id, project.id)"
            @rename="renaming = $event"
            @delete-backup="(id) => $emit('deleteBackup', project.id, id)"
          />

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

    <!-- 行内改名：右键菜单选中「重命名」后弹出，DialogPortal 挂到 body -->
    <ConversationRenameDialog
      :visible="renaming !== null"
      :conversation="renaming"
      @close="renaming = null"
    />
  </aside>
</template>

<script setup lang="ts">
import { useLocalStorage } from "@vueuse/core";
import { computed, ref, watch } from "vue";
import ConversationRenameDialog from "@/components/sidebar/ConversationRenameDialog.vue";
import ThreadItem from "@/components/sidebar/ThreadItem.vue";
import {
  buildThreadTree,
  groupThreadsByUpdatedAt,
} from "@/components/sidebar/threads";
import type { components } from "@/api/schema";
import type { ThreadNode } from "@/components/sidebar/threads";

type Project = components["schemas"]["Project"];
type Conversation = components["schemas"]["Conversation"];

const props = defineProps<{
  projects: readonly Project[];
  conversations: ReadonlyMap<string, readonly Conversation[]>;
  selectedConversationId: string | null;
}>();

const emit = defineEmits<{
  selectConversation: [conversationId: string, projectId: string];
  createConversation: [projectId: string];
  deleteBackup: [projectId: string, conversationId: string];
}>();

const expandedProjects = ref<Set<string>>(new Set());
const filter = ref("");
// 视图偏好持久化，下次打开保持（与 oct-theme 同一套 localStorage 约定）
const sortByTime = useLocalStorage("oct-sidebar-sort-by-time", false);

// 正在改名的会话；非 null 即弹窗打开，置 null 即关闭（取消/Esc/成功统一走这里）
const renaming = ref<Conversation | null>(null);

function matchesFilter(conversation: Conversation) {
  const query = filter.value.trim().toLowerCase();
  return (conversation.title || "New Chat").toLowerCase().includes(query);
}

// 搜索时隐藏没有匹配对话的项目分组
const visibleProjects = computed(() => {
  if (!filter.value.trim()) return props.projects;
  return props.projects.filter((project) =>
    (props.conversations.get(project.id) ?? []).some(matchesFilter),
  );
});

/**
 * 按时间视图：全部项目的会话合并（列表本就按 updated_at DESC），套搜索
 * 过滤后按最后更新时间分组成 今天/昨天/星期X/月份。
 */
const timeGroups = computed(() => {
  const flat = props.projects.flatMap(
    (project) => props.conversations.get(project.id) ?? [],
  );
  const matched = filter.value.trim() ? flat.filter(matchesFilter) : flat;
  return groupThreadsByUpdatedAt(matched).map((group) => ({
    label: group.label,
    nodes: group.conversations.map(
      (conv): ThreadNode => ({ conv, children: [] }),
    ),
  }));
});

const projectNames = computed(
  () => new Map(props.projects.map((project) => [project.id, project.name])),
);

/**
 * 项目的线程树。无过滤词时按 fork/回退备份嵌套；有过滤词时退回平铺的
 * 命中行——嵌套树里「父不匹配但子匹配」的展示规则太绕，搜索结果平铺
 * 最可预期。
 */
function threadsOf(projectId: string): ThreadNode[] {
  const list = props.conversations.get(projectId) ?? [];
  if (!filter.value.trim()) return buildThreadTree(list);
  return list
    .filter(matchesFilter)
    .map((conversation) => ({ conv: conversation, children: [] }));
}

function toggleProject(id: string) {
  const next = new Set(expandedProjects.value);
  if (next.has(id)) next.delete(id);
  else next.add(id);
  expandedProjects.value = next;
}

// 按时间视图的行不带项目上下文，从会话归属反查项目 id（不在任何列表则忽略）
function resolveProjectId(conversationId: string): string | null {
  for (const [projectId, list] of props.conversations) {
    if (list.some((conversation) => conversation.id === conversationId)) {
      return projectId;
    }
  }
  return null;
}

function handleSelect(conversationId: string) {
  const projectId = resolveProjectId(conversationId);
  if (projectId) emit("selectConversation", conversationId, projectId);
}

function handleDeleteBackup(conversationId: string) {
  const projectId = resolveProjectId(conversationId);
  if (projectId) emit("deleteBackup", projectId, conversationId);
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
