<template>
  <!-- 通用标签条：chat / 文件 / diff / git graph 等所有标签类型共用 -->
  <div
    class="[scrollbar-width:none] h-[35px] flex flex-none items-stretch overflow-x-auto border-b border-line-soft bg-titlebar [&::-webkit-scrollbar]:hidden"
  >
    <button
      v-for="tab in tabs"
      :key="tab.id"
      class="relative flex items-center gap-[7px] whitespace-nowrap border-r border-line-soft py-0 pl-3 pr-[6px] text-[12.5px] transition-colors"
      :class="
        tab.active
          ? 'bg-background text-strong shadow-[inset_0_2px_0_var(--oct-color-accent-7)]'
          : 'text-dim hover:bg-neutral-1 hover:text-neutral-10'
      "
      :title="tab.title"
      @click="$emit('select', tab.id)"
    >
      <i-lucide-message-square
        v-if="tab.kind === 'chat'"
        class="h-3 w-3 flex-none"
      />
      <i-lucide-file
        v-else-if="tab.kind === 'file'"
        class="h-3 w-3 flex-none"
      />
      <i-lucide-git-compare
        v-else-if="tab.kind === 'diff'"
        class="h-3 w-3 flex-none"
      />
      <i-lucide-git-branch v-else class="h-3 w-3 flex-none" />
      <span class="max-w-[160px] truncate">{{ tab.title }}</span>
      <span
        class="h-4 w-4 inline-flex items-center justify-center rounded-[4px] text-faint transition-colors hover:bg-neutral-2 hover:text-neutral-10"
        title="关闭标签"
        @click.stop="$emit('close', tab.id)"
      >
        <i-lucide-x class="h-2.5 w-2.5" />
      </span>
    </button>

    <!-- 新建标签：不同类型的标签从这里打开 -->
    <DropdownMenuRoot>
      <DropdownMenuTrigger
        class="w-[34px] flex flex-none items-center justify-center text-faint outline-none transition-colors hover:bg-neutral-1 hover:text-neutral-10"
        title="新建标签"
      >
        <i-lucide-plus class="h-3 w-3" />
      </DropdownMenuTrigger>
      <DropdownMenuPortal>
        <DropdownMenuContent
          align="start"
          :side-offset="6"
          class="z-50 min-w-[150px] border border-neutral-5 rounded-lg bg-surface p-1 shadow-[var(--oct-shadow)]"
        >
          <DropdownMenuItem
            v-for="action in addActions"
            :key="action.label"
            class="h-[26px] flex cursor-pointer items-center gap-2 rounded-[5px] px-2 text-[12.5px] text-neutral-10 outline-none data-[highlighted]:bg-neutral-1"
            @select="$emit('tabAction', action.event)"
          >
            <component :is="action.icon" class="h-3.5 w-3.5 text-dim" />
            {{ action.label }}
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenuPortal>
    </DropdownMenuRoot>
  </div>
</template>

<script setup lang="ts">
import {
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuPortal,
  DropdownMenuRoot,
  DropdownMenuTrigger,
} from "reka-ui";
import IconGraph from "~icons/lucide/git-branch";
import IconDiff from "~icons/lucide/git-compare";
import IconChat from "~icons/lucide/message-square";
import type { TabKind } from "@/composables/useTabs";
import type { Component } from "vue";

export interface DisplayTab {
  id: string;
  kind: TabKind;
  title: string;
  active: boolean;
}

defineProps<{
  tabs: DisplayTab[];
}>();

defineEmits<{
  select: [tabId: string];
  close: [tabId: string];
  tabAction: [action: TabAction];
}>();

export type TabAction = "add-chat" | "open-diff" | "open-graph";

const addActions: {
  label: string;
  event: TabAction;
  icon: Component;
}[] = [
  { label: "New Chat", event: "add-chat", icon: IconChat },
  { label: "File Diff", event: "open-diff", icon: IconDiff },
  { label: "Git Graph", event: "open-graph", icon: IconGraph },
];
</script>
