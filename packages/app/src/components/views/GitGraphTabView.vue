<template>
  <section class="workspace-panel min-w-0 flex-1 bg-background">
    <header
      class="h-9 flex flex-none items-center gap-2 border-b border-line-soft px-4 text-base text-neutral-10 font-semibold"
    >
      <i-lucide-git-branch class="h-3.5 w-3.5 text-dim" />
      Git Graph
    </header>
    <div
      class="flex flex-1 flex-col items-center justify-center gap-2 px-6 text-center text-faint"
    >
      <i-lucide-git-branch class="h-8 w-8" />
      <p class="text-base text-dim">Git Graph 视图即将上线</p>
      <p class="text-sm">Git 数据 API 接入后，这里将展示分支与提交图</p>
    </div>
  </section>
</template>
