<template>
  <!-- 审批条：pending 审批逐条卡片垂直堆叠于输入区上方。
       数据源是 useChat 会话的 pendingApprovals 分桶（事件 upsert / GET
       快照整表替换两路维护），decide 经 emit 交给页面层调 decideApproval -->
  <div class="flex-none px-7 pt-2.5">
    <TransitionGroup
      tag="div"
      name="approval-card"
      class="mx-auto max-w-[760px] space-y-2"
    >
      <div
        v-for="approval in approvals"
        :key="approval.approvalId"
        class="border border-warning-6 rounded-[10px] bg-warning-1 px-3 py-2.5"
      >
        <!-- 头部：kind 图标/文案 + 工作区外徽标 + 摘要（命令行或路径） -->
        <div class="min-w-0 flex items-center gap-2">
          <span class="h-4 w-4 flex-none text-warning-10">
            <component
              :is="kindMeta(approval.kind).icon"
              class="block h-full w-full"
            />
          </span>
          <span class="flex-none text-[12.5px] text-strong font-semibold">
            {{ kindMeta(approval.kind).label }}
          </span>
          <span
            v-if="approval.agentId"
            class="max-w-[180px] truncate text-[11px] text-warning-10"
            :title="approval.agentId"
            >子代理 ·
            {{
              taskByAgentId(approval.agentId)?.description ?? approval.agentId
            }}</span
          >
          <span
            v-if="approval.outside"
            class="h-[16px] inline-flex flex-none items-center whitespace-nowrap rounded-full bg-warning-7 px-1.5 text-[10.5px] text-warning-10 font-medium leading-none"
            title="目标路径解析后位于工作区（含附加目录）之外"
          >
            工作区外
          </span>
          <span
            class="min-w-0 flex-1 truncate text-[12px] text-dim font-mono"
            :title="approval.path ?? approval.command ?? approval.title"
          >
            {{ approval.title }}
          </span>
        </div>

        <!-- pattern 输入：默认值 = rule_suggestion，仅 allow_always 提交时
             携带；MCP 工具的规则不带 pattern（后端强制 NULL），不渲染 -->
        <div
          v-if="approval.kind !== 'mcp'"
          class="mt-2 min-w-0 flex items-center gap-2"
        >
          <span class="flex-none text-[11px] text-faint">规则 pattern</span>
          <input
            :value="patternOf(approval)"
            type="text"
            class="h-7 min-w-0 flex-1 border border-line-soft rounded-md bg-surface px-2 text-[11.5px] text-neutral-10 font-mono outline-none transition-colors focus:border-accent-6 placeholder:text-faint"
            placeholder="绝对路径 glob / 命令前缀（可编辑后随「总是允许」保存）"
            @input="
              setPattern(approval, ($event.target as HTMLInputElement).value)
            "
          />
        </div>

        <!-- 三档决定：允许一次 / 总是允许（落规则再放行）/ 拒绝 -->
        <div class="mt-2 flex items-center gap-1.5">
          <span class="flex-1" />
          <button
            type="button"
            class="h-7 flex-none rounded-[7px] bg-danger-7 px-2.5 text-[12px] text-danger-10 font-medium control-focus transition-[filter] hover:brightness-110"
            title="拒绝本次调用（模型可见拒绝原因）"
            @click="decide(approval, 'deny')"
          >
            拒绝
          </button>
          <button
            type="button"
            class="h-7 flex-none border border-neutral-5 rounded-[7px] bg-surface px-2.5 text-[12px] text-neutral-10 font-medium control-focus transition-colors hover:bg-neutral-3"
            title="允许并在本项目保存一条 allow 规则（pattern 见上方输入框）"
            @click="decide(approval, 'allow_always')"
          >
            总是允许
          </button>
          <button
            type="button"
            class="h-7 flex-none rounded-[7px] bg-accent-7 px-2.5 text-[12px] text-accent-contrast font-medium control-focus transition-[filter] hover:brightness-110"
            title="仅允许本次调用"
            @click="decide(approval, 'allow')"
          >
            允许一次
          </button>
        </div>
      </div>
    </TransitionGroup>
  </div>
</template>

<script setup lang="ts">
import { reactive } from "vue";
import IconFilePen from "~icons/lucide/file-pen";
import IconFilePenLine from "~icons/lucide/file-pen-line";
import IconFileText from "~icons/lucide/file-text";
import IconTerminal from "~icons/lucide/terminal";
import IconWrench from "~icons/lucide/wrench";
import { taskByAgentId } from "@/composables/useSubagents";
import type { ApprovalDecision, PendingApproval } from "@/composables/useChat";
import type { Component } from "vue";

defineProps<{
  /** 当前会话的待决审批（useChat 会话的 pendingApprovals 分桶）。 */
  approvals: readonly PendingApproval[];
}>();

const emit = defineEmits<{
  decide: [approvalId: string, decision: ApprovalDecision, pattern?: string];
}>();

/** kind → 图标/文案；未知 kind 原样展示（工具名兜底）。 */
const KIND_META: Record<string, { icon: Component; label: string }> = {
  // 读类审批只在工作区外触发（决策表：区内读恒 Allow）
  read: { icon: IconFileText, label: "读取工作区外" },
  write: { icon: IconFilePen, label: "写入文件" },
  edit: { icon: IconFilePenLine, label: "编辑文件" },
  command: { icon: IconTerminal, label: "执行命令" },
  mcp: { icon: IconWrench, label: "MCP 工具" },
};

function kindMeta(kind: string): { icon: Component; label: string } {
  return KIND_META[kind] ?? { icon: IconWrench, label: kind };
}

// pattern 编辑按 approvalId 存本地：pending 列表 upsert 覆盖（事件重放）
// 不冲掉用户已输入的值；未编辑时回退 rule_suggestion 默认值。
const patterns = reactive(new Map<string, string>());

function patternOf(approval: PendingApproval): string {
  return patterns.get(approval.approvalId) ?? approval.ruleSuggestion ?? "";
}

function setPattern(approval: PendingApproval, value: string) {
  patterns.set(approval.approvalId, value);
}

function decide(approval: PendingApproval, decision: ApprovalDecision) {
  // pattern 只随 allow_always 提交（allow/deny 不落规则）；MCP 规则无
  // pattern，空串不提交（后端回退 suggestion，再退化为一次性放行）。
  const pattern =
    decision === "allow_always" && approval.kind !== "mcp"
      ? patternOf(approval).trim() || undefined
      : undefined;
  emit("decide", approval.approvalId, decision, pattern);
}
</script>

<style scoped>
/* 卡片进出的轻过渡（UnoCSS 不覆盖 TransitionGroup 的类名， scoped 手写） */
.approval-card-enter-active {
  transition:
    opacity 0.15s ease-out,
    transform 0.15s ease-out;
}
.approval-card-enter-from {
  opacity: 0;
  transform: translateY(-4px);
}
.approval-card-leave-active {
  transition: opacity 0.1s ease-in;
}
.approval-card-leave-to {
  opacity: 0;
}
.approval-card-move {
  transition: transform 0.15s ease;
}
</style>
