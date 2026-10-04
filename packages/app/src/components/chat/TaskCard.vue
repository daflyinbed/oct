<template>
  <!-- 子代理任务卡片：父消息里 task 工具调用的专属展示（step 行风格）。
       运行中即可「打开子代理」（agent_id 来自任务 store），不必等结果 -->
  <div
    class="min-w-0 border rounded-[10px] bg-panel"
    :class="frameClass"
    data-testid="task-card"
  >
    <div class="flex items-center gap-2 px-3 py-1.5">
      <i-lucide-bot class="h-3.5 w-3.5 flex-none" :class="statusClass" />
      <span class="flex-none text-[12px] font-medium" :class="statusClass"
        >子代理</span
      >
      <!-- worker 角色标记：角色由宿主固定，不由模型选择 -->
      <span
        class="h-[15px] inline-flex flex-none items-center whitespace-nowrap rounded-full bg-neutral-2 px-1.5 text-[10px] text-dim leading-none font-mono"
        >worker</span
      >
      <span
        class="min-w-0 flex-1 truncate text-[12px] text-strong"
        :title="state.description"
        >{{ state.description }}</span
      >
      <span
        v-if="elapsed"
        class="flex-none text-[11px] text-faint tabular-nums"
        >{{ elapsed }}</span
      >
      <!-- 状态 chip：运行中 spinner / 完成 ✓ / 失败 ✗ / 取消 / 待恢复 -->
      <span v-if="part.inherited" class="text-[11px] text-faint"
        >来源会话历史任务</span
      >
      <span
        v-else-if="state.status === 'running'"
        class="flex flex-none items-center gap-1"
      >
        <i-lucide-loader-circle
          class="h-3.5 w-3.5 animate-spin text-accent-7"
        />
        <span class="text-[11px] text-accent-10">运行中</span>
      </span>
      <span
        v-else-if="state.status === 'completed'"
        class="h-[16px] inline-flex flex-none items-center whitespace-nowrap rounded-full bg-success-7 px-1.5 text-[10.5px] text-success-10 font-medium leading-none"
        >✓ 已完成</span
      >
      <span
        v-else-if="state.status === 'failed'"
        class="h-[16px] inline-flex flex-none items-center whitespace-nowrap rounded-full bg-danger-7 px-1.5 text-[10.5px] text-danger-10 font-medium leading-none"
        >✗ 失败</span
      >
      <span
        v-else-if="state.status === 'cancelled'"
        class="h-[16px] inline-flex flex-none items-center whitespace-nowrap rounded-full bg-neutral-3 px-1.5 text-[10.5px] text-dim font-medium leading-none"
        >已取消</span
      >
      <span
        v-else
        class="h-[16px] inline-flex flex-none items-center whitespace-nowrap rounded-full bg-warning-7 px-1.5 text-[10.5px] text-warning-10 font-medium leading-none"
        >待恢复</span
      >
      <!-- 打开子代理：持久入口（agent_id 未知前禁用） -->
      <button
        v-if="!part.inherited"
        type="button"
        class="h-[22px] inline-flex flex-none items-center gap-1 border border-line-soft rounded-[6px] px-1.5 text-[11px] text-dim control-focus transition-colors hover:bg-neutral-1 hover:text-neutral-10 disabled:opacity-50"
        :disabled="state.agentId == null"
        :title="
          state.agentId == null
            ? '等待子代理启动…'
            : `打开子代理 · ${state.description}`
        "
        data-testid="task-card-open"
        @click="open"
      >
        <i-lucide-external-link class="h-3 w-3" />
        打开子代理
      </button>
    </div>

    <!-- 运行中的活动摘要（正在执行的命令 / 读取的文件） -->
    <div
      v-if="state.activity && state.status === 'running'"
      class="flex items-center gap-1.5 border-t border-line-soft px-3 py-1 text-[11.5px] text-faint"
    >
      <i-lucide-terminal class="h-3 w-3 flex-none" />
      <span class="min-w-0 flex-1 truncate">{{ state.activity }}</span>
    </div>

    <!-- 结果摘要：信封 JSON 的 output 头部（或原文头部） -->
    <div
      v-if="summary"
      class="border-t border-line-soft px-3 py-1.5 text-[11.5px] leading-[1.55]"
      :class="part.isError ? 'text-danger-10' : 'text-dim'"
    >
      {{ summary }}
    </div>
  </div>
</template>

<script setup lang="ts">
import { useNow } from "@vueuse/core";
import { computed } from "vue";
import {
  formatSubagentElapsed,
  openSubagentPanel,
  subagentStatusClass,
  taskByCallId,
} from "@/composables/useSubagents";
import { resolveTaskCard, taskResultSummary } from "./taskCard";
import type { ToolCallPart } from "@/composables/useChat";

const props = defineProps<{
  part: ToolCallPart;
  /** 所属根会话 id：「打开子代理」标签需要它支持「返回主对话」。 */
  conversationId: string | null;
}>();

// 运行中的耗时需要每秒走表；useNow 随组件作用域自动停止
const now = useNow({ interval: 1000 });

// 任务 store 命中（live）：subagent_updated 事件落地后即可拿到
// agent_id / 状态 / 活动；历史回放则由 details 兜底（resolveTaskCard 内）。
const task = computed(() =>
  props.part.inherited ? null : taskByCallId(props.part.callId),
);

const state = computed(() => resolveTaskCard(props.part, task.value));
const statusClass = computed(() => subagentStatusClass(state.value.status));

const elapsed = computed(() =>
  formatSubagentElapsed(
    state.value.createdAt,
    state.value.finishedAt,
    now.value.valueOf(),
  ),
);

const summary = computed(() =>
  state.value.status === "running"
    ? null
    : taskResultSummary(props.part.content),
);

const frameClass = computed(() => {
  if (state.value.status === "failed") return "border-danger-6";
  if (state.value.status === "lost") return "border-warning-6";
  return "border-line-soft";
});

function open() {
  if (props.part.inherited) return;
  const agentId = state.value.agentId;
  if (!agentId) return;
  openSubagentPanel(agentId, state.value.description, props.conversationId);
}
</script>
