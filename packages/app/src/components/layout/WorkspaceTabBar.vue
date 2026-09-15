<template>
  <!-- 通用标签条：chat / 文件 / diff / git graph 等所有标签类型共用 -->
  <div
    class="h-[35px] flex-none flex items-stretch bg-titlebar border-b border-line-soft overflow-x-auto [scrollbar-width:none] [&::-webkit-scrollbar]:hidden"
  >
    <button
      v-for="tab in tabs"
      :key="tab.id"
      class="flex items-center gap-[7px] py-0 pl-3 pr-[6px] border-r border-line-soft text-[12.5px] whitespace-nowrap relative transition-colors"
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
        class="w-3 h-3 flex-none"
      />
      <i-lucide-file
        v-else-if="tab.kind === 'file'"
        class="w-3 h-3 flex-none"
      />
      <i-lucide-git-compare
        v-else-if="tab.kind === 'diff'"
        class="w-3 h-3 flex-none"
      />
      <i-lucide-git-branch v-else class="w-3 h-3 flex-none" />
      <span class="max-w-[160px] truncate">{{ tab.title }}</span>
      <span
        class="w-4 h-4 rounded-[4px] inline-flex items-center justify-center text-faint hover:bg-neutral-2 hover:text-neutral-10 transition-colors"
        title="关闭标签"
        @click.stop="$emit('close', tab.id)"
      >
        <i-lucide-x class="w-2.5 h-2.5" />
      </span>
    </button>

    <!-- 新建标签：不同类型的标签从这里打开 -->
    <DropdownMenuRoot>
      <DropdownMenuTrigger
        class="w-[34px] flex-none flex items-center justify-center text-faint hover:bg-neutral-1 hover:text-neutral-10 transition-colors outline-none"
        title="新建标签"
      >
        <i-lucide-plus class="w-3 h-3" />
      </DropdownMenuTrigger>
      <DropdownMenuPortal>
        <DropdownMenuContent
          align="start"
          :side-offset="6"
          class="min-w-[150px] z-50 p-1 rounded-lg border border-neutral-5 bg-surface shadow-[var(--oct-shadow)]"
        >
          <DropdownMenuItem
            v-for="action in addActions"
            :key="action.label"
            class="flex items-center gap-2 h-[26px] px-2 rounded-[5px] text-[12.5px] text-neutral-10 outline-none cursor-pointer data-[highlighted]:bg-neutral-1"
            @select="$emit('tab-action', action.event)"
          >
            <component :is="action.icon" class="w-3.5 h-3.5 text-dim" />
            {{ action.label }}
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenuPortal>
    </DropdownMenuRoot>
  </div>
</template>

<script setup lang="ts">
import type { Component } from "vue";
import {
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuPortal,
  DropdownMenuRoot,
  DropdownMenuTrigger,
} from "reka-ui";
import type { TabKind } from "@/composables/useTabs";
import IconChat from "~icons/lucide/message-square";
import IconDiff from "~icons/lucide/git-compare";
import IconGraph from "~icons/lucide/git-branch";

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
  "tab-action": [action: TabAction];
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
