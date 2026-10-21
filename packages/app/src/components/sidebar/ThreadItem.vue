<template>
  <div class="space-y-px">
    <!-- ContextMenuRoot 不渲染 DOM，行结构与样式保持不变；右键触发，不拦截左键点击 -->
    <ContextMenuRoot>
      <ContextMenuTrigger as-child>
        <button
          class="relative w-full flex rounded-md pr-2 text-left text-base control-focus transition-colors"
          :class="[
            layout === 'time'
              ? 'flex-col items-start gap-[3px] py-[7px]'
              : 'items-center py-[5px]',
            depth === 0 || layout === 'time' ? 'pl-[26px]' : 'pl-2',
            selected
              ? 'bg-accent-3 text-neutral-10'
              : 'text-dim hover:bg-neutral-1 hover:text-neutral-10',
          ]"
          :title="rowTitle"
          @click="$emit('select', node.conv.id)"
        >
          <span
            v-if="selected"
            class="absolute top-1/2 h-3.5 w-[3px] rounded-full bg-accent-7 -translate-y-1/2"
            :class="depth === 0 || layout === 'time' ? 'left-2' : 'left-0'"
          />
          <!-- 按时间视图：标题行 + 项目行两行（日期分组的组头已表达时间，行内不再放） -->
          <template v-if="layout === 'time'">
            <span class="min-w-0 w-full flex items-center gap-[7px]">
              <component
                :is="kindIcon"
                v-if="kindIcon"
                class="h-[11px] w-[11px] flex-none text-faint"
              />
              <span class="min-w-0 flex-1 truncate text-strong font-semibold">{{
                node.conv.title || "New Chat"
              }}</span>
              <PermissionBadge :mode="node.conv.permission_mode" />
            </span>
            <span
              class="max-w-full flex items-center gap-[5px] text-xs text-faint"
            >
              <i-lucide-folder class="h-[11px] w-[11px] flex-none" />
              <span class="truncate">{{ projectName }}</span>
            </span>
          </template>
          <template v-else>
            <!-- 类型图标：分叉 / 回退备份（源会话被删后退回顶层的孤儿行同样带图标） -->
            <component
              :is="kindIcon"
              v-if="kindIcon"
              class="h-[11px] w-[11px] flex-none text-faint"
            />
            <span class="min-w-0 flex-1 truncate">{{
              node.conv.title || "New Chat"
            }}</span>
            <PermissionBadge :mode="node.conv.permission_mode" />
            <!-- aria-hidden：时间只是补充元数据（悬停看绝对时间），保持行名 = 标题，
                 与标签页按钮同名以便按名定位 -->
            <span
              class="flex-none text-badge text-faint"
              :title="new Date(node.conv.updated_at).toLocaleString()"
              aria-hidden="true"
              >{{ relativeThreadTime(node.conv.updated_at) }}</span
            >
          </template>
        </button>
      </ContextMenuTrigger>
      <ContextMenuPortal>
        <ContextMenuContent
          class="z-50 min-w-[150px] border border-neutral-5 rounded-lg bg-surface p-1 shadow-[var(--oct-shadow)]"
        >
          <ContextMenuItem
            class="h-[26px] flex cursor-pointer items-center gap-2 rounded-[5px] px-2 text-base text-neutral-10 outline-none data-[highlighted]:bg-neutral-1"
            @select="$emit('rename', node.conv)"
          >
            <i-lucide-pencil class="h-3.5 w-3.5 text-dim" />
            重命名
          </ContextMenuItem>
          <ContextMenuItem
            v-if="node.conv.origin_kind === 'revert_backup'"
            class="h-[26px] flex cursor-pointer items-center gap-2 rounded-[5px] px-2 text-base text-danger-10 outline-none data-[highlighted]:bg-neutral-1"
            @select="$emit('deleteBackup', node.conv.id)"
          >
            <i-lucide-trash-2 class="h-3.5 w-3.5" />删除备份
          </ContextMenuItem>
        </ContextMenuContent>
      </ContextMenuPortal>
    </ContextMenuRoot>

    <!-- 分叉/回退备份子线程：树形缩进 + 引导线，递归支持任意深度 -->
    <div
      v-if="node.children.length > 0"
      class="ml-[18px] border-l border-line-soft pl-1 space-y-px"
    >
      <ThreadItem
        v-for="child in node.children"
        :key="child.conv.id"
        :node="child"
        :selected-id="selectedId"
        :depth="depth + 1"
        @select="(id) => $emit('select', id)"
        @rename="(conversation) => $emit('rename', conversation)"
        @delete-backup="(id) => $emit('deleteBackup', id)"
      />
    </div>
  </div>
</template>

<script setup lang="ts">
import {
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuPortal,
  ContextMenuRoot,
  ContextMenuTrigger,
} from "reka-ui";
import { computed } from "vue";
import IconGitBranch from "~icons/lucide/git-branch";
import IconRotateCcw from "~icons/lucide/rotate-ccw";
import PermissionBadge from "@/components/sidebar/PermissionBadge.vue";
import { relativeThreadTime } from "@/components/sidebar/threads";
import type { components } from "@/api/schema";
import type { ThreadNode } from "@/components/sidebar/threads";

type Conversation = components["schemas"]["Conversation"];

const props = defineProps<{
  node: ThreadNode;
  selectedId: string | null;
  /** 0 = 项目分组顶层行；≥1 = 分叉/备份的嵌套层级（决定缩进内的排布） */
  depth?: number;
  /** tree = 项目视图行（默认）；time = 按时间视图的两行行（标题 + 项目） */
  layout?: "tree" | "time";
  /** time 布局第二行显示的项目名 */
  projectName?: string;
}>();

defineEmits<{
  select: [conversationId: string];
  rename: [conversation: Conversation];
  deleteBackup: [conversationId: string];
}>();

const depth = computed(() => props.depth ?? 0);
const selected = computed(() => props.selectedId === props.node.conv.id);

// 行级提示：分叉/回退备份标来源语义；回退备份额外说明只读。
const rowTitle = computed(() => {
  if (props.node.conv.origin_kind === "fork") return "分叉对话";
  if (props.node.conv.origin_kind === "revert_backup")
    return "回退备份（只读）";
  return undefined;
});

const kindIcon = computed(() => {
  if (props.node.conv.origin_kind === "fork") return IconGitBranch;
  if (props.node.conv.origin_kind === "revert_backup") return IconRotateCcw;
  return null;
});
</script>
