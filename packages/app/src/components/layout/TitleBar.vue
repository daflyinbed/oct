<template>
  <header
    class="h-10 flex-none flex items-center gap-1 bg-titlebar border-b border-line-soft pl-1.5 pr-2.5 select-none"
  >
    <div class="flex items-center gap-0.5 min-w-0">
      <!-- 应用菜单：项目操作 + 设置入口（抄自 demo #menuDD） -->
      <DropdownMenuRoot>
        <DropdownMenuTrigger
          class="control-ghost control-focus w-7 h-7 rounded-md inline-flex items-center justify-center flex-none"
          title="菜单"
        >
          <i-lucide-menu class="w-4 h-4" />
        </DropdownMenuTrigger>
        <DropdownMenuPortal>
          <DropdownMenuContent
            align="start"
            :side-offset="6"
            class="min-w-[210px] z-50 p-1 rounded-lg border border-neutral-5 bg-surface shadow-[var(--oct-shadow)]"
          >
            <DropdownMenuItem
              class="flex items-center gap-2 h-[26px] px-2 rounded-[5px] text-[12.5px] text-neutral-10 outline-none cursor-pointer data-[highlighted]:bg-neutral-1"
              @select="$emit('create-project')"
            >
              <i-lucide-plus class="w-3.5 h-3.5 text-dim" />
              New Project…
            </DropdownMenuItem>
            <DropdownMenuItem
              class="flex items-center gap-2 h-[26px] px-2 rounded-[5px] text-[12.5px] text-neutral-10 outline-none cursor-pointer data-[highlighted]:bg-neutral-1"
              @select="$emit('create-project')"
            >
              <i-lucide-download class="w-3.5 h-3.5 text-dim" />
              Import Project…
            </DropdownMenuItem>
            <DropdownMenuSeparator class="h-px bg-line-soft my-1 mx-1" />
            <DropdownMenuItem
              class="flex items-center gap-2 h-[26px] px-2 rounded-[5px] text-[12.5px] text-neutral-10 outline-none cursor-pointer data-[highlighted]:bg-neutral-1"
              @select="$emit('open-settings')"
            >
              <i-lucide-sliders-horizontal class="w-3.5 h-3.5 text-dim" />
              Providers &amp; Models…
              <kbd
                class="ml-auto font-mono text-[10px] text-faint border border-line-soft rounded-[4px] px-1"
                >⌘,</kbd
              >
            </DropdownMenuItem>
            <DropdownMenuItem
              class="flex items-center gap-2 h-[26px] px-2 rounded-[5px] text-[12.5px] text-neutral-10 outline-none cursor-pointer data-[highlighted]:bg-neutral-1"
              @select="$emit('open-settings')"
            >
              <i-lucide-settings class="w-3.5 h-3.5 text-dim" />
              Settings…
            </DropdownMenuItem>
            <DropdownMenuSeparator class="h-px bg-line-soft my-1 mx-1" />
            <DropdownMenuItem
              class="flex items-center gap-2 h-[26px] px-2 rounded-[5px] text-[12.5px] text-neutral-10 outline-none cursor-pointer data-[highlighted]:bg-neutral-1"
              @select="focusComposer"
            >
              <i-lucide-keyboard class="w-3.5 h-3.5 text-dim" />
              Keyboard Shortcuts
              <kbd
                class="ml-auto font-mono text-[10px] text-faint border border-line-soft rounded-[4px] px-1"
                >⌘K</kbd
              >
            </DropdownMenuItem>
            <DropdownMenuItem
              class="flex items-center gap-2 h-[26px] px-2 rounded-[5px] text-[12.5px] text-neutral-10 outline-none cursor-pointer data-[highlighted]:bg-neutral-1"
            >
              <i-lucide-info class="w-3.5 h-3.5 text-dim" />
              About Oct
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenuPortal>
      </DropdownMenuRoot>

      <span
        class="flex items-center gap-[7px] px-2 pl-1 text-[13px] font-semibold tracking-[0.2px] text-strong"
      >
        <svg
          class="text-accent-7"
          width="15"
          height="15"
          viewBox="0 0 16 16"
          fill="currentColor"
        >
          <circle cx="8" cy="8" r="6.5" opacity=".35" />
          <circle cx="8" cy="8" r="3" />
        </svg>
        oct
      </span>

      <!-- 项目切换器 -->
      <DropdownMenuRoot>
        <DropdownMenuTrigger
          class="inline-flex items-center gap-1.5 h-[26px] px-2 rounded-md text-[12.5px] font-medium text-neutral-10 outline-none hover:bg-neutral-1 data-[state=open]:bg-neutral-1 transition-colors"
          :title="activeProject?.working_dir"
        >
          <i-lucide-folder class="w-3.5 h-3.5 text-dim" />
          <span class="max-w-[180px] truncate">{{
            activeProject?.name ?? "oct"
          }}</span>
          <i-lucide-chevron-down class="w-2.5 h-2.5 text-faint" />
        </DropdownMenuTrigger>
        <DropdownMenuPortal>
          <DropdownMenuContent
            align="start"
            :side-offset="6"
            class="min-w-[220px] z-50 p-1 rounded-lg border border-neutral-5 bg-surface shadow-[var(--oct-shadow)]"
          >
            <div
              class="px-2 pt-1 pb-0.5 text-[10.5px] font-semibold tracking-[0.6px] uppercase text-faint"
            >
              切换项目
            </div>
            <DropdownMenuItem
              v-for="project in projects"
              :key="project.id"
              class="flex items-center gap-2 h-[26px] px-2 rounded-[5px] text-[12.5px] text-neutral-10 outline-none cursor-pointer data-[highlighted]:bg-neutral-1"
              @select="$emit('select-project', project.id)"
            >
              <span class="truncate">{{ project.name }}</span>
              <span
                class="ml-auto flex-none font-mono text-[11px] text-faint truncate max-w-[140px]"
                >{{ project.working_dir }}</span
              >
            </DropdownMenuItem>
            <DropdownMenuSeparator class="h-px bg-line-soft my-1 mx-1" />
            <DropdownMenuItem
              class="flex items-center gap-2 h-[26px] px-2 rounded-[5px] text-[12.5px] text-neutral-10 outline-none cursor-pointer data-[highlighted]:bg-neutral-1"
              @select="$emit('create-project')"
            >
              <i-lucide-plus class="w-3.5 h-3.5 text-dim" />
              New Project…
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenuPortal>
      </DropdownMenuRoot>
    </div>

    <div class="flex-1" />

    <!-- 居中命令搜索（聚焦输入框） -->
    <button
      class="flex items-center gap-2 w-[340px] max-w-[38vw] h-[26px] px-2.5 rounded-md border border-line-soft bg-panel text-faint text-[12px] hover:border-line hover:text-dim transition-colors"
      @click="focusComposer"
    >
      <i-lucide-search class="w-3 h-3 flex-none" />
      <span class="truncate">搜索对话、文件、命令…</span>
      <kbd
        class="ml-auto font-mono text-[10.5px] text-faint border border-line-soft rounded-[4px] px-1"
        >⌘K</kbd
      >
    </button>

    <div class="flex-1" />

    <div class="flex items-center gap-0.5">
      <button
        class="control-ghost control-focus w-7 h-7 rounded-md"
        title="Provider 设置"
        @click="$emit('open-settings')"
      >
        <i-lucide-settings class="w-4 h-4" />
      </button>
      <span
        class="w-6 h-6 ml-1 rounded-full bg-gradient-to-br from-accent-7 to-accent-9 text-white text-[11px] font-semibold inline-flex items-center justify-center"
        title="账户"
      >
        O
      </span>
    </div>
  </header>
</template>

<script setup lang="ts">
import { computed } from "vue";
import {
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuPortal,
  DropdownMenuRoot,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "reka-ui";
import type { components } from "@/api/schema";

type Project = components["schemas"]["Project"];

const props = defineProps<{
  projects: readonly Project[];
  activeProjectId: string | null;
}>();

defineEmits<{
  "select-project": [projectId: string];
  "create-project": [];
  "open-settings": [];
}>();

const activeProject = computed(() =>
  props.projects.find((project) => project.id === props.activeProjectId),
);

function focusComposer() {
  // 菜单关闭时 reka-ui 会在下一帧把焦点还给触发按钮，排在其后同帧再聚焦输入框
  requestAnimationFrame(() => {
    window.dispatchEvent(new CustomEvent("oct:focus-composer"));
  });
}
</script>
