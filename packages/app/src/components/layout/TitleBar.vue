<template>
  <header
    class="h-10 flex flex-none select-none items-center gap-1 border-b border-line-soft bg-titlebar pl-1.5 pr-2.5"
  >
    <div class="min-w-0 flex items-center gap-0.5">
      <!-- 应用菜单：项目操作 + 设置入口（抄自 demo #menuDD） -->
      <DropdownMenuRoot>
        <DropdownMenuTrigger
          class="h-7 w-7 inline-flex flex-none items-center justify-center control-ghost rounded-md control-focus"
          title="菜单"
        >
          <i-lucide-menu class="h-4 w-4" />
        </DropdownMenuTrigger>
        <DropdownMenuPortal>
          <DropdownMenuContent
            align="start"
            :side-offset="6"
            class="z-50 min-w-[210px] border border-neutral-5 rounded-lg bg-surface p-1 shadow-[var(--oct-shadow)]"
          >
            <DropdownMenuItem
              class="h-[26px] flex cursor-pointer items-center gap-2 rounded-[5px] px-2 text-[12.5px] text-neutral-10 outline-none data-[highlighted]:bg-neutral-1"
              @select="$emit('createProject')"
            >
              <i-lucide-plus class="h-3.5 w-3.5 text-dim" />
              New Project…
            </DropdownMenuItem>
            <DropdownMenuItem
              class="h-[26px] flex cursor-pointer items-center gap-2 rounded-[5px] px-2 text-[12.5px] text-neutral-10 outline-none data-[highlighted]:bg-neutral-1"
              @select="$emit('createProject')"
            >
              <i-lucide-download class="h-3.5 w-3.5 text-dim" />
              Import Project…
            </DropdownMenuItem>
            <DropdownMenuSeparator class="mx-1 my-1 h-px bg-line-soft" />
            <DropdownMenuItem
              class="h-[26px] flex cursor-pointer items-center gap-2 rounded-[5px] px-2 text-[12.5px] text-neutral-10 outline-none data-[highlighted]:bg-neutral-1"
              @select="$emit('openSettings')"
            >
              <i-lucide-sliders-horizontal class="h-3.5 w-3.5 text-dim" />
              Providers &amp; Models…
              <kbd
                class="ml-auto border border-line-soft rounded-[4px] px-1 text-[10px] text-faint font-mono"
                >⌘,</kbd
              >
            </DropdownMenuItem>
            <DropdownMenuItem
              class="h-[26px] flex cursor-pointer items-center gap-2 rounded-[5px] px-2 text-[12.5px] text-neutral-10 outline-none data-[highlighted]:bg-neutral-1"
              @select="$emit('openSettings')"
            >
              <i-lucide-settings class="h-3.5 w-3.5 text-dim" />
              Settings…
            </DropdownMenuItem>
            <DropdownMenuSeparator class="mx-1 my-1 h-px bg-line-soft" />
            <DropdownMenuItem
              class="h-[26px] flex cursor-pointer items-center gap-2 rounded-[5px] px-2 text-[12.5px] text-neutral-10 outline-none data-[highlighted]:bg-neutral-1"
              @select="focusComposer"
            >
              <i-lucide-keyboard class="h-3.5 w-3.5 text-dim" />
              Keyboard Shortcuts
              <kbd
                class="ml-auto border border-line-soft rounded-[4px] px-1 text-[10px] text-faint font-mono"
                >⌘K</kbd
              >
            </DropdownMenuItem>
            <DropdownMenuItem
              class="h-[26px] flex cursor-pointer items-center gap-2 rounded-[5px] px-2 text-[12.5px] text-neutral-10 outline-none data-[highlighted]:bg-neutral-1"
            >
              <i-lucide-info class="h-3.5 w-3.5 text-dim" />
              About Oct
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenuPortal>
      </DropdownMenuRoot>

      <span
        class="flex items-center gap-[7px] px-2 pl-1 text-[13px] text-strong font-semibold tracking-[0.2px]"
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
          class="h-[26px] inline-flex items-center gap-1.5 rounded-md px-2 text-[12.5px] text-neutral-10 font-medium outline-none transition-colors data-[state=open]:bg-neutral-1 hover:bg-neutral-1"
          :title="activeProject?.working_dir"
        >
          <i-lucide-folder class="h-3.5 w-3.5 text-dim" />
          <span class="max-w-[180px] truncate">{{
            activeProject?.name ?? "oct"
          }}</span>
          <i-lucide-chevron-down class="h-2.5 w-2.5 text-faint" />
        </DropdownMenuTrigger>
        <DropdownMenuPortal>
          <DropdownMenuContent
            align="start"
            :side-offset="6"
            class="z-50 min-w-[220px] border border-neutral-5 rounded-lg bg-surface p-1 shadow-[var(--oct-shadow)]"
          >
            <div
              class="px-2 pb-0.5 pt-1 text-[10.5px] text-faint font-semibold tracking-[0.6px] uppercase"
            >
              切换项目
            </div>
            <DropdownMenuItem
              v-for="project in projects"
              :key="project.id"
              class="h-[26px] flex cursor-pointer items-center gap-2 rounded-[5px] px-2 text-[12.5px] text-neutral-10 outline-none data-[highlighted]:bg-neutral-1"
              @select="$emit('selectProject', project.id)"
            >
              <span class="truncate">{{ project.name }}</span>
              <span
                class="ml-auto max-w-[140px] flex-none truncate text-[11px] text-faint font-mono"
                >{{ project.working_dir }}</span
              >
            </DropdownMenuItem>
            <DropdownMenuSeparator class="mx-1 my-1 h-px bg-line-soft" />
            <DropdownMenuItem
              class="h-[26px] flex cursor-pointer items-center gap-2 rounded-[5px] px-2 text-[12.5px] text-neutral-10 outline-none data-[highlighted]:bg-neutral-1"
              @select="$emit('createProject')"
            >
              <i-lucide-plus class="h-3.5 w-3.5 text-dim" />
              New Project…
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenuPortal>
      </DropdownMenuRoot>
    </div>

    <div class="flex-1" />

    <!-- 居中命令搜索（聚焦输入框） -->
    <button
      class="h-[26px] max-w-[38vw] w-[340px] flex items-center gap-2 border border-line-soft rounded-md bg-panel px-2.5 text-[12px] text-faint transition-colors hover:border-line hover:text-dim"
      @click="focusComposer"
    >
      <i-lucide-search class="h-3 w-3 flex-none" />
      <span class="truncate">搜索对话、文件、命令…</span>
      <kbd
        class="ml-auto border border-line-soft rounded-[4px] px-1 text-[10.5px] text-faint font-mono"
        >⌘K</kbd
      >
    </button>

    <div class="flex-1" />

    <div class="flex items-center gap-0.5">
      <button
        class="h-7 w-7 control-ghost rounded-md control-focus"
        title="Provider 设置"
        @click="$emit('openSettings')"
      >
        <i-lucide-settings class="h-4 w-4" />
      </button>
      <span
        class="ml-1 h-6 w-6 inline-flex items-center justify-center rounded-full from-accent-7 to-accent-9 bg-gradient-to-br text-[11px] text-white font-semibold"
        title="账户"
      >
        O
      </span>
    </div>
  </header>
</template>

<script setup lang="ts">
import {
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuPortal,
  DropdownMenuRoot,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "reka-ui";
import { computed } from "vue";
import type { components } from "@/api/schema";

type Project = components["schemas"]["Project"];

const props = defineProps<{
  projects: readonly Project[];
  activeProjectId: string | null;
}>();

defineEmits<{
  selectProject: [projectId: string];
  createProject: [];
  openSettings: [];
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
