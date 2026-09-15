<template>
  <footer
    class="h-[30px] flex-none flex items-center gap-0.5 border-t border-line-soft bg-titlebar px-2 select-none"
  >
    <button
      class="control-ghost control-focus inline-flex items-center justify-center w-[26px] h-[22px] rounded-[5px] transition-colors"
      :class="sidebarVisible ? 'text-accent-10' : ''"
      title="切换侧栏"
      @click="$emit('toggle-sidebar')"
    >
      <i-lucide-panel-left class="w-3.5 h-3.5" />
    </button>
    <button
      class="control-ghost control-focus inline-flex items-center justify-center w-[26px] h-[22px] rounded-[5px] transition-colors"
      :class="chatVisible ? 'text-accent-10' : ''"
      title="切换聊天区"
      @click="$emit('toggle-chat')"
    >
      <i-lucide-message-square class="w-3.5 h-3.5" />
    </button>

    <span class="w-px h-3.5 bg-line mx-1.5" />
    <span class="flex items-center gap-1.5 px-1 text-faint text-[11.5px]">
      <span class="text-success-10">✓</span> Ready
    </span>

    <div class="flex-1" />

    <button
      class="control-ghost control-focus inline-flex items-center justify-center w-[26px] h-[22px] rounded-[5px] transition-colors"
      :title="isDark ? '切换亮色主题' : '切换暗色主题'"
      @click="toggleTheme"
    >
      <i-lucide-sun v-if="isDark" class="w-3.5 h-3.5" />
      <i-lucide-moon v-else class="w-3.5 h-3.5" />
    </button>

    <span class="w-px h-3.5 bg-line mx-1.5" />
    <button
      class="control-ghost control-focus inline-flex items-center justify-center w-[26px] h-[22px] rounded-[5px] transition-colors"
      :class="diffVisible ? 'text-accent-10' : ''"
      title="Changes 面板"
      @click="$emit('toggle-diff')"
    >
      <i-lucide-git-compare class="w-3.5 h-3.5" />
    </button>
    <button
      class="control-ghost control-focus inline-flex items-center justify-center w-[26px] h-[22px] rounded-[5px] transition-colors"
      :class="fileTreeVisible ? 'text-accent-10' : ''"
      title="Files 面板"
      @click="$emit('toggle-file-tree')"
    >
      <i-lucide-files class="w-3.5 h-3.5" />
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
  "toggle-sidebar": [];
  "toggle-chat": [];
  "toggle-diff": [];
  "toggle-file-tree": [];
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
