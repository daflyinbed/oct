<template>
  <!-- 锚在输入框容器内的绝对定位下拉:焦点始终留在 textarea(键盘导航由
       宿主转发),不接管焦点,因此不经 popover 组件;参数表单等真正模态
       交互走 PromptArgsDialog(reka Dialog) -->
  <div
    class="absolute bottom-full left-0 right-0 z-30 mb-2 max-h-[280px] overflow-y-auto border border-neutral-5 rounded-lg bg-surface p-1.5 shadow-[var(--oct-shadow)]"
    role="listbox"
  >
    <template v-for="group in groups" :key="group.label">
      <p
        class="px-2 pb-0.5 pt-1.5 text-[10.5px] text-faint font-medium tracking-wide uppercase"
      >
        {{ group.label }}
      </p>
      <button
        v-for="item in group.items"
        :key="item.key"
        type="button"
        role="option"
        :aria-selected="isActive(item)"
        class="w-full flex items-center gap-2 rounded-md px-2 py-1.5 text-left transition-colors"
        :class="isActive(item) ? 'bg-neutral-2' : ''"
        @mousedown.prevent="$emit('select', item)"
        @mouseenter="hoveredKey = item.key"
        @mouseleave="hoveredKey = null"
      >
        <i-lucide-terminal
          v-if="item.kind === 'builtin'"
          class="h-3.5 w-3.5 flex-none text-dim"
        />
        <i-lucide-message-square-quote
          v-else
          class="h-3.5 w-3.5 flex-none text-dim"
        />
        <span class="min-w-0 flex-1 truncate">
          <span
            v-if="item.kind === 'builtin'"
            class="text-[12.5px] text-neutral-10 font-mono"
            >/{{ item.label }}</span
          >
          <span v-else class="text-[12.5px] text-neutral-10 font-mono">{{
            item.prompt.name
          }}</span>
          <span
            v-if="itemDescription(item)"
            class="ml-1.5 text-[11.5px] text-dim"
            >{{ itemDescription(item) }}</span
          >
        </span>
        <span
          v-if="item.kind === 'prompt' && item.prompt.arguments.length > 0"
          class="flex-none text-[10.5px] text-faint"
        >
          {{ item.prompt.arguments.length }} 个参数
        </span>
      </button>
    </template>
  </div>
</template>

<script setup lang="ts">
import { ref } from "vue";
import type {
  SlashMenuGroup,
  SlashMenuItem,
} from "@/composables/useSlashCommands";

const props = defineProps<{
  groups: readonly SlashMenuGroup[];
  /** 键盘导航的当前项 key（宿主持有扁平索引）;本地悬停可临时覆盖高亮。 */
  activeKey: string | null;
}>();

defineEmits<{
  select: [item: SlashMenuItem];
}>();

const hoveredKey = ref<string | null>(null);

/** 高亮项 = 键盘当前项,悬停时切换为悬停项(悬停移开即回到键盘项)。 */
function isActive(item: SlashMenuItem): boolean {
  return item.key === (hoveredKey.value ?? props.activeKey);
}

function itemDescription(item: SlashMenuItem): string | null {
  if (item.kind === "builtin") return item.description;
  return item.prompt.description ?? null;
}
</script>
