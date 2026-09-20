<template>
  <footer
    class="h-[30px] flex flex-none select-none items-center gap-0.5 border-t border-line-soft bg-titlebar px-2"
  >
    <button
      class="h-[22px] w-[26px] inline-flex items-center justify-center control-ghost rounded-[5px] control-focus transition-colors"
      :class="sidebarVisible ? 'text-accent-10' : ''"
      title="切换侧栏"
      @click="$emit('toggleSidebar')"
    >
      <i-lucide-panel-left class="h-3.5 w-3.5" />
    </button>
    <button
      class="h-[22px] w-[26px] inline-flex items-center justify-center control-ghost rounded-[5px] control-focus transition-colors"
      :class="chatVisible ? 'text-accent-10' : ''"
      title="切换聊天区"
      @click="$emit('toggleChat')"
    >
      <i-lucide-message-square class="h-3.5 w-3.5" />
    </button>

    <span class="mx-1.5 h-3.5 w-px bg-line" />
    <span class="flex items-center gap-1.5 px-1 text-[11.5px] text-faint">
      <span class="text-success-10">✓</span> Ready
    </span>

    <div class="flex-1" />

    <button
      class="h-[22px] w-[26px] inline-flex items-center justify-center control-ghost rounded-[5px] control-focus transition-colors"
      :title="isDark ? '切换亮色主题' : '切换暗色主题'"
      @click="toggleTheme"
    >
      <i-lucide-sun v-if="isDark" class="h-3.5 w-3.5" />
      <i-lucide-moon v-else class="h-3.5 w-3.5" />
    </button>

    <span class="mx-1.5 h-3.5 w-px bg-line" />
    <button
      class="h-[22px] w-[26px] inline-flex items-center justify-center control-ghost rounded-[5px] control-focus transition-colors"
      :class="diffVisible ? 'text-accent-10' : ''"
      title="Changes 面板"
      @click="$emit('toggleDiff')"
    >
      <i-lucide-git-compare class="h-3.5 w-3.5" />
    </button>
    <button
      class="h-[22px] w-[26px] inline-flex items-center justify-center control-ghost rounded-[5px] control-focus transition-colors"
      :class="fileTreeVisible ? 'text-accent-10' : ''"
      title="Files 面板"
      @click="$emit('toggleFileTree')"
    >
      <i-lucide-files class="h-3.5 w-3.5" />
    </button>
  </footer>
</template>

<script setup lang="ts">
import { useLocalStorage } from "@vueuse/core";
import { computed, watchEffect } from "vue";

defineProps<{
  sidebarVisible: boolean;
  chatVisible: boolean;
  diffVisible: boolean;
  fileTreeVisible: boolean;
}>();

defineEmits<{
  toggleSidebar: [];
  toggleChat: [];
  toggleDiff: [];
  toggleFileTree: [];
}>();

// 主题持久化到 localStorage，index.html 里有首帧前的恢复脚本
const theme = useLocalStorage<"light" | "dark">("oct-theme", "light");
const isDark = computed(() => theme.value === "dark");

watchEffect(() => {
  document.documentElement.dataset.theme = theme.value;
});

function toggleTheme() {
  theme.value = theme.value === "dark" ? "light" : "dark";
}
</script>
