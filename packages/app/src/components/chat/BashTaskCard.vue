<template>
  <!-- 后台任务卡：execute_command 的 run_in_background 信封（tool_result
       details.kind === "background_task"）不再走通用终端卡片——命令已交给
       后台运行时，卡片展示任务状态而非命令输出。live 数据源是任务快照
       store（信封里的 task_id → 轮询维护的快照），离线/历史回放兜底信封
       自身的元数据（status 恒为 running，等待快照对账修正） -->
  <div
    class="min-w-0 border rounded-[10px] bg-panel transition-colors"
    :class="frameClass"
    :data-bg-task-id="taskId"
  >
    <div class="flex items-center gap-2 px-3 py-1.5">
      <i-lucide-terminal
        class="h-3.5 w-3.5 flex-none"
        :class="backgroundStatusClass(state.status)"
      />
      <span
        class="flex-none text-sm font-medium"
        :class="backgroundStatusClass(state.status)"
        >后台任务</span
      >
      <!-- 命令行（terminal 样式）：cwd 弱化 + $ 提示符 + 命令 -->
      <span
        class="min-w-0 flex-1 truncate text-code text-strong font-mono"
        :title="commandLine"
      >
        <span v-if="workingDir" class="text-accent-10" :title="workingDir">{{
          shortCwd(workingDir)
        }}</span
        ><span class="text-faint">$ </span>{{ command }}
      </span>

      <!-- 运行中：耗时走表 + 输出字节数 -->
      <span v-if="elapsed" class="flex-none text-xs text-faint tabular-nums">{{
        elapsed
      }}</span>
      <span
        v-if="state.status === 'running' && state.outputBytes != null"
        class="flex-none text-xs text-faint tabular-nums"
        >{{ formatBackgroundBytes(state.outputBytes) }}</span
      >

      <!-- 状态 chip -->
      <span v-if="part.inherited" class="text-xs text-faint"
        >来源会话历史任务</span
      >
      <span
        v-else-if="state.status === 'running'"
        class="flex flex-none items-center gap-1"
      >
        <i-lucide-loader-circle
          class="h-3.5 w-3.5 animate-spin text-accent-7"
        />
        <span class="text-xs text-accent-10">运行中</span>
      </span>
      <span
        v-else-if="state.status === 'completed'"
        class="h-[16px] inline-flex flex-none items-center whitespace-nowrap rounded-full px-1.5 text-badge font-medium leading-none"
        :class="
          state.exitCode === 0
            ? 'bg-success-7 text-success-10'
            : 'bg-danger-7 text-danger-10'
        "
        >{{
          state.exitCode === 0
            ? "✓ 完成"
            : state.exitCode != null
              ? `✓ exit ${state.exitCode}`
              : "✓ 完成"
        }}</span
      >
      <span
        v-else-if="state.status === 'failed'"
        class="h-[16px] inline-flex flex-none items-center whitespace-nowrap rounded-full bg-danger-7 px-1.5 text-badge text-danger-10 font-medium leading-none"
        >✗ 失败</span
      >
      <span
        v-else-if="state.status === 'cancelled'"
        class="h-[16px] inline-flex flex-none items-center whitespace-nowrap rounded-full bg-neutral-3 px-1.5 text-badge text-dim font-medium leading-none"
        >已取消</span
      >
      <span
        v-else
        class="h-[16px] inline-flex flex-none items-center whitespace-nowrap rounded-full bg-warning-7 px-1.5 text-badge text-warning-10 font-medium leading-none"
        title="后端重启时进程被终结"
        >已中断</span
      >

      <!-- 停止：仅运行中可用（click 不冒泡展开） -->
      <button
        v-if="!part.inherited && state.status === 'running'"
        type="button"
        class="h-[22px] inline-flex flex-none items-center gap-1 border border-line-soft rounded-[6px] px-1.5 text-xs text-dim control-focus transition-colors hover:bg-danger-1 hover:text-danger-10 disabled:opacity-50"
        :disabled="stopping"
        title="停止此后台任务"
        @click.stop="stop"
      >
        <i-lucide-square class="h-3 w-3 fill-current" />
        {{ stopping ? "停止中…" : "停止" }}
      </button>

      <!-- 查看输出：懒加载（首次点击才请求尾部），Dialog 展示 -->
      <button
        v-if="!part.inherited"
        type="button"
        class="h-[22px] inline-flex flex-none items-center gap-1 border border-line-soft rounded-[6px] px-1.5 text-xs text-dim control-focus transition-colors hover:bg-neutral-1 hover:text-neutral-10"
        title="查看输出文件"
        @click.stop="showOutput = true"
      >
        <i-lucide-scroll-text class="h-3 w-3" />
        查看输出
      </button>
    </div>

    <!-- 失败原因（机制性击杀：输出超限 / 24h 上限） -->
    <div
      v-if="state.error"
      class="border-t border-line-soft px-3 py-1 text-xs text-danger-10"
    >
      {{ state.error }}
    </div>
  </div>

  <BashTaskOutputDialog
    v-if="showOutput"
    :task-id="taskId"
    :command="command"
    @close="showOutput = false"
  />
</template>

<script setup lang="ts">
import { useIntervalFn, useNow } from "@vueuse/core";
import { computed, onMounted, ref } from "vue";
import {
  backgroundStatusClass,
  backgroundTaskById,
  cancelBackgroundTask,
  formatBackgroundBytes,
  isBackgroundTaskHighlighted,
  isBackgroundTaskStatus,
  refreshBackgroundTasks,
} from "@/composables/useBackgroundTasks";
import { formatSubagentElapsed } from "@/composables/useSubagents";
import BashTaskOutputDialog from "./BashTaskOutputDialog.vue";
import type { BackgroundTaskStatus } from "@/composables/useBackgroundTasks";
import type { ToolCallPart } from "@/composables/useChat";

const props = defineProps<{
  part: ToolCallPart;
  /** 信封所在会话：快照对账的作用域。 */
  conversationId: string | null;
}>();

// --- 信封元数据（details.kind === "background_task"，live/stored 同形） ----

const envelope = computed(() => props.part.details);
const taskId = computed(() => {
  const id = envelope.value?.task_id;
  return typeof id === "string" ? id : "";
});

// --- 数据源：任务快照 store（live 权威）→ 信封兜底（恒 running） ------------

const task = computed(() =>
  !props.part.inherited && taskId.value
    ? backgroundTaskById(taskId.value)
    : null,
);

const state = computed(() => {
  const envStatus = envelope.value?.status;
  const status: BackgroundTaskStatus =
    task.value?.status ??
    (isBackgroundTaskStatus(envStatus) ? envStatus : "running");
  return {
    status,
    exitCode: task.value?.exit_code ?? null,
    outputBytes: task.value?.output_bytes ?? null,
    error: task.value?.error ?? null,
  };
});

const command = computed(() => {
  const raw = envelope.value?.command;
  if (typeof raw === "string" && raw) return raw;
  // 信封缺 command（理论不发生）：回退解析工具参数
  try {
    const parsed = props.part.arguments
      ? (JSON.parse(props.part.arguments) as Record<string, unknown>)
      : null;
    if (parsed && typeof parsed.command === "string") return parsed.command;
  } catch {
    // 半截 JSON：留空
  }
  return "";
});

const workingDir = computed(() => {
  const raw = envelope.value?.working_dir;
  return typeof raw === "string" && raw ? raw : null;
});

const commandLine = computed(() =>
  workingDir.value ? `${workingDir.value}$ ${command.value}` : command.value,
);

// 运行中的耗时需要每秒走表；useNow 随组件作用域自动停止
const now = useNow({ scheduler: (update) => useIntervalFn(update, 1000) });
const elapsed = computed(() =>
  task.value
    ? formatSubagentElapsed(
        task.value.created_at,
        task.value.finished_at ?? null,
        now.value.valueOf(),
      )
    : null,
);

// 高亮（通知分隔条/底部指示器的「查看任务」定位）：切 accent 边框 + 背景闪
const highlighted = computed(() => isBackgroundTaskHighlighted(taskId.value));

const frameClass = computed(() => {
  if (highlighted.value) return "border-accent-6 bg-accent-1";
  if (state.value.status === "failed") return "border-danger-6";
  if (state.value.status === "lost") return "border-warning-6";
  return "border-line-soft";
});

// --- 停止 / 查看输出 ---------------------------------------------------------

const stopping = ref(false);
const showOutput = ref(false);

async function stop(): Promise<void> {
  if (props.part.inherited || !taskId.value || stopping.value) return;
  stopping.value = true;
  try {
    // 终态行就地 upsert，卡片/指示器立即收敛；404（已结束）忽略即可。
    await cancelBackgroundTask(taskId.value);
  } finally {
    stopping.value = false;
  }
}

// 快照对账兜底：live 信封到达（本卡片挂载）或历史回放时 store 可能还没这份
// 任务（App.vue 的会话级 refresh 在途/缺席）——拉一次列表，顺带启动轮询。
// 每实例一次，避免响应性循环。
let ensured = false;
onMounted(() => {
  if (props.part.inherited) return;
  if (ensured) return;
  ensured = true;
  if (
    props.conversationId &&
    taskId.value &&
    backgroundTaskById(taskId.value) == null
  ) {
    void refreshBackgroundTasks(props.conversationId);
  }
});

// 提示符里的 cwd 过长时只留尾部一段，完整路径由 hover（title）展示
const CWD_TAIL_CHARS = 28;

function shortCwd(cwd: string): string {
  return cwd.length > CWD_TAIL_CHARS ? `…${cwd.slice(-CWD_TAIL_CHARS)}` : cwd;
}
</script>
