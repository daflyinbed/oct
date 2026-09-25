<template>
  <!-- 后台任务通知分隔条（ADR 0003）：任务终态后后端合批插入的 user 行，
       渲染为全宽分隔条（CompactionDivider 的接入方式）。摘要行按批内最差
       状态聚合；展开后按任务列出状态/退出码/命令/内联输出尾部，并提供
       「查看任务」定位到消息流里对应的信封卡片 -->
  <div class="border border-line-soft rounded-[10px] bg-panel">
    <button
      type="button"
      class="w-full flex items-center gap-2 rounded-[10px] px-3 py-2 text-left control-focus"
      :title="open ? '收起详情' : '展开详情'"
      @click="open = !open"
    >
      <i-lucide-bell class="h-3.5 w-3.5 flex-none text-faint" />
      <span class="flex-none text-[12px] font-medium" :class="summaryClass">{{
        summaryText
      }}</span>
      <span
        class="min-w-0 flex-1 truncate text-[12px] text-dim font-mono"
        :title="firstTask?.command"
        >{{ firstTask?.command }}</span
      >
      <span class="flex flex-none items-center gap-1 text-[11px] text-faint">
        {{ part.tasks.length > 1 ? `${part.tasks.length} 个任务` : "后台任务" }}
        <i-lucide-chevron-down
          class="h-3 w-3 transition-transform"
          :class="open ? 'rotate-180' : ''"
        />
      </span>
    </button>
    <div v-if="open" class="border-t border-line-soft px-3 py-2 space-y-2.5">
      <div v-for="task in part.tasks" :key="task.taskId" class="min-w-0">
        <div class="flex items-center gap-2">
          <span
            class="h-1.5 w-1.5 flex-none rounded-full"
            :class="dotOf(task.status)"
          />
          <span class="flex-none text-[11.5px]" :class="labelClassOf(task)">
            {{ labelOf(task) }}
          </span>
          <span
            class="min-w-0 flex-1 truncate text-[11.5px] text-dim font-mono"
            :title="task.command"
            >{{ task.command }}</span
          >
          <!-- 定位信封卡片：高亮 + 滚动（简单实现：DOM 查询 + scrollIntoView） -->
          <button
            type="button"
            class="h-[22px] inline-flex flex-none items-center gap-1 border border-line-soft rounded-[6px] px-1.5 text-[11px] text-dim control-focus transition-colors hover:bg-neutral-1 hover:text-neutral-10"
            title="定位到任务卡片"
            @click="locate(task)"
          >
            <i-lucide-crosshair class="h-3 w-3" />
            查看任务
          </button>
        </div>
        <!-- 通知内联的输出尾部（后端截 4KiB，可能带省略前缀） -->
        <pre
          v-if="task.tail"
          class="mt-1 max-h-56 overflow-auto whitespace-pre-wrap break-words rounded-md bg-surface px-2.5 py-1.5 text-[11px] text-dim leading-[1.5] font-mono"
          >{{ task.tail }}</pre
        >
        <p v-if="task.error" class="mt-1 text-[11px] text-danger-10">
          {{ task.error }}
        </p>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed, ref } from "vue";
import {
  backgroundStatusClass,
  backgroundStatusDot,
  backgroundStatusLabel,
  highlightBackgroundTask,
  isBackgroundTaskStatus,
} from "@/composables/useBackgroundTasks";
import type {
  TaskNotificationPart,
  TaskNotificationTask,
} from "@/composables/useChat";

const props = defineProps<{
  part: TaskNotificationPart;
}>();

const open = ref(false);

const firstTask = computed(() => props.part.tasks[0] ?? null);

/** 批内最差状态（lost > failed > cancelled > completed）。 */
const worstStatus = computed(() => {
  const statuses = props.part.tasks.map((task) => task.status);
  if (statuses.includes("lost")) return "lost";
  if (statuses.includes("failed")) return "failed";
  if (statuses.includes("cancelled")) return "cancelled";
  return "completed";
});

/** 摘要文案：已完成带退出码摘要；非零退出以 danger 色标注。 */
const summaryText = computed(() => {
  const tasks = props.part.tasks;
  const prefix = tasks.length > 1 ? `${tasks.length} 个后台任务` : "后台任务";
  switch (worstStatus.value) {
    case "lost":
      return `${prefix}已中断（后端重启）`;
    case "failed":
      return `${prefix}已失败`;
    case "cancelled":
      return `${prefix}已取消`;
    default: {
      const nonzero = tasks.filter(
        (task) => task.status === "completed" && task.exitCode !== 0,
      );
      if (tasks.length === 1 && tasks[0]!.exitCode != null) {
        return `${prefix}已完成 · exit ${tasks[0]!.exitCode}`;
      }
      if (nonzero.length > 0) {
        return `${prefix}已完成 · ${nonzero.length} 个非零退出`;
      }
      return `${prefix}已完成 · exit 0`;
    }
  }
});

const summaryClass = computed(
  () =>
    ({
      lost: "text-warning-10",
      failed: "text-danger-10",
      cancelled: "text-faint",
      completed: "text-success-10",
    })[worstStatus.value] ?? "text-dim",
);

function dotOf(status: string): string {
  return isBackgroundTaskStatus(status)
    ? backgroundStatusDot(status)
    : "bg-neutral-5";
}

function labelOf(task: TaskNotificationTask): string {
  const label = isBackgroundTaskStatus(task.status)
    ? backgroundStatusLabel(task.status)
    : task.status;
  if (
    task.status === "completed" &&
    task.exitCode != null &&
    task.exitCode !== 0
  ) {
    return `${label} · exit ${task.exitCode}`;
  }
  return label;
}

function labelClassOf(task: TaskNotificationTask): string {
  if (
    task.status === "completed" &&
    task.exitCode !== 0 &&
    task.exitCode != null
  ) {
    return "text-danger-10";
  }
  return isBackgroundTaskStatus(task.status)
    ? backgroundStatusClass(task.status)
    : "text-faint";
}

/** 定位：store 高亮（BashTaskCard 切 accent 边框）+ 平滑滚动到卡片。 */
function locate(task: TaskNotificationTask): void {
  highlightBackgroundTask(task.taskId);
  const el = document.querySelector(
    `[data-bg-task-id="${CSS.escape(task.taskId)}"]`,
  );
  el?.scrollIntoView({ behavior: "smooth", block: "center" });
}
</script>
