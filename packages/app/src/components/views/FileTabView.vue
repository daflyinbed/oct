<template>
  <section class="workspace-panel min-w-0 flex-1 bg-background">
    <header
      class="h-9 flex-none flex items-center gap-2 px-4 border-b border-line-soft font-mono text-[12.5px] text-dim"
    >
      <i-lucide-file class="w-3.5 h-3.5 flex-none text-faint" />
      <span class="truncate">{{ path }}</span>
    </header>
    <div
      class="flex-1 flex flex-col items-center justify-center gap-2 px-6 text-center text-faint"
    >
      <i-lucide-file-text class="w-8 h-8" />
      <p class="text-[13px] text-dim">文件内容预览即将支持</p>
      <p class="text-[12px]">
        文件读取 API 接入后，这里将展示
        <span class="font-mono">{{ path }}</span> 的内容
      </p>
    </div>
  </section>
</template>

<script setup lang="ts">
defineProps<{
  path: string;
}>();
</script>
