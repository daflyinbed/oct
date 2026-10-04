<template>
  <!-- 子代理完整面板：与主 panel 共用消息展示链路（ChatMessageList 及其
       子组件），但持有独立实例化的会话状态——不碰 useChat 单例，usage
       只进实例本地快照（ContextRing 经 props 喂入）。输入区被任务状态
       栏替代（受管查看模式：不支持发送/模型切换/resume） -->
  <main class="workspace-panel min-w-0 flex-1 bg-background">
    <header class="workspace-panel-header gap-2 text-[12.5px] text-neutral-10">
      <i-lucide-bot class="h-3.5 w-3.5 flex-none text-dim" />
      <span class="min-w-0 truncate font-semibold">{{ identity }}</span>
      <span
        class="h-[15px] inline-flex flex-none items-center whitespace-nowrap rounded-full bg-neutral-2 px-1.5 text-[10px] text-dim leading-none font-mono"
        >worker</span
      >
      <span class="flex-1" />
      <span
        v-if="task?.model_id"
        class="max-w-[220px] flex-none truncate text-[11px] text-faint font-mono"
        :title="task.model_id"
      >
        {{ task.model_id }}
      </span>
      <span
        class="flex flex-none items-center gap-1.5 text-[11.5px]"
        :title="statusTitle"
      >
        <i-lucide-loader-circle
          v-if="isRunning"
          class="h-3 w-3 animate-spin text-accent-7"
        />
        <span v-else class="h-1.5 w-1.5 rounded-full" :class="statusDot" />
        <span :class="statusClass">{{ statusText }}</span>
      </span>
      <!-- 返回主对话：激活（或补开）根会话的 chat 标签；App 的标签同步
           watch 会负责路由跳转/刷新 -->
      <button
        v-if="rootConversationId"
        class="h-[24px] inline-flex flex-none items-center gap-1.5 control-surface rounded-md px-2 text-[11.5px] font-medium control-focus"
        title="返回主对话"
        @click="returnToParent"
      >
        <i-lucide-arrow-left class="h-3 w-3" />
        返回主对话
      </button>
    </header>

    <details
      v-if="task?.agent_config"
      class="flex-none border-b border-line-soft px-7 py-2 text-[11.5px] text-dim"
    >
      <summary class="cursor-pointer">
        任务配置 · {{ task.agent_config.tools.length }} 个工具
      </summary>
      <p class="mt-2 whitespace-pre-wrap">
        {{ task.agent_config.instructions || "无额外角色指令" }}
      </p>
      <p class="mt-1 break-all font-mono">
        {{
          task.agent_config.tools.map((tool) => tool.exposed_name).join(", ") ||
          "无工具"
        }}
      </p>
    </details>

    <ChatMessageList
      :messages="messages"
      :conversation-id="agentId"
      empty-title="子代理尚未产生消息"
      empty-hint="等待委派 prompt 落地…"
    />

    <ApprovalBanner
      v-if="approvals.length > 0"
      :approvals="approvals"
      @decide="
        (id, decision, pattern) =>
          decideApproval(agentId, id, decision, pattern)
      "
    />

    <!-- 状态栏（替代输入区）：任务状态 + 子会话上下文用量 + 停止/结束提示 -->
    <div class="flex-none px-7 pb-3.5 pt-2.5">
      <div
        class="mx-auto max-w-[760px] flex items-center gap-2.5 border border-line-soft rounded-[10px] bg-surface px-3 py-2"
      >
        <span class="min-w-0 flex-1 text-[12.5px]" :class="statusClass">
          <template v-if="isRunning"
            >子代理运行中 · 查看模式（不能从此处发送消息）</template
          >
          <template v-else-if="task?.status === 'lost'">
            执行实例已丢失（待恢复） · 请在主对话使用「恢复此轮」
          </template>
          <template v-else
            >任务已结束（{{ statusText }}） · 仅展示历史</template
          >
        </span>
        <!-- 子会话自己的上下文占用环（实例本地 usage，props 喂入） -->
        <ContextRing
          :input-tokens="usage.inputTokens.value"
          :window-tokens="usage.windowTokens.value"
          :compacting="usage.compacting.value"
        />
        <button
          v-if="isRunning"
          type="button"
          class="h-7 inline-flex flex-none items-center justify-center rounded-[7px] bg-neutral-3 text-neutral-10 control-focus transition-colors hover:bg-neutral-4"
          title="停止此子任务（不影响其他任务）"
          data-testid="subagent-stop"
          @click="stop"
        >
          <i-lucide-square class="h-3 w-3 fill-current" />
        </button>
      </div>
    </div>
  </main>
</template>

<script setup lang="ts">
import { computed, onBeforeUnmount, watch } from "vue";
import ApprovalBanner from "@/components/chat/ApprovalBanner.vue";
import ChatMessageList from "@/components/chat/ChatMessageList.vue";
import ContextRing from "@/components/chat/ContextRing.vue";
import { createChatSession } from "@/composables/useChat";
import {
  cancelSubagent,
  subagentStatusClass,
  subagentStatusDot,
  subagentStatusLabel,
  taskByAgentId,
} from "@/composables/useSubagents";
import { useTabs } from "@/composables/useTabs";

const props = defineProps<{
  /** 子会话 id（= 子 conversation id）。 */
  agentId: string;
  /** 所属根会话 id（「返回主对话」）；null = 未知（隐藏入口）。 */
  rootConversationId: string | null;
  /** 标签兜底标题（任务 store 命中时用实时 description）。 */
  description: string;
}>();

// 实例化会话：contextUsage=false（子 usage 不进全局单例），不接线
// subagent 事件派发（子 hub 不发该事件）。
const session = createChatSession({ contextUsage: false });
const {
  messages,
  sending,
  usage,
  pendingApprovals,
  decideApproval,
  attachSubagent,
  dispose,
} = session;
const approvals = computed(
  () => pendingApprovals.value.get(props.agentId) ?? [],
);

// 任务 store（模块级）：面板打开时通常已由根会话快照/事件维护
const task = computed(() => taskByAgentId(props.agentId));

const identity = computed(
  () => task.value?.description?.trim() || props.description || "子代理",
);

// 运行判定：store 命中看任务状态；未命中（如快照未含该 agent）退回
// 实例的流式状态（正在重放/订阅 = 有活动 run）。
const isRunning = computed(
  () =>
    task.value?.status === "running" || (task.value == null && sending.value),
);

const statusDot = computed(() =>
  task.value ? subagentStatusDot(task.value.status) : "bg-neutral-5",
);
const statusClass = computed(() =>
  task.value ? subagentStatusClass(task.value.status) : "text-faint",
);
const statusText = computed(() =>
  task.value
    ? subagentStatusLabel(task.value.status)
    : sending.value
      ? "运行中"
      : "已结束",
);
const statusTitle = computed(
  () =>
    `run ${task.value?.run_id ?? "未知"} · epoch ${task.value?.execution_epoch ?? "-"}`,
);

// 挂载即拉历史 + 探测活动 run；agentId 变化（理论上不会——标签按
// agentId 去重）时重新挂载。
watch(
  () => props.agentId,
  (agentId) => {
    void attachSubagent(agentId);
  },
  { immediate: true },
);

// 关闭标签 = 关闭视图（中断订阅），绝不取消任务；后端执行不受影响。
onBeforeUnmount(() => {
  dispose();
});

function stop() {
  void cancelSubagent(props.agentId);
}

function returnToParent() {
  const root = props.rootConversationId;
  if (!root) return;
  // openTab 按 id 去重：已有则激活，没有则补开；App.vue 的 activeTabId
  // watch 随后把路由切回该会话（并在需要时刷新主面板）。
  const { openTab } = useTabs();
  openTab({ id: `chat:${root}`, kind: "chat", payload: root });
}
</script>
