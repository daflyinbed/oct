<template>
  <!-- 压缩标记：全宽分隔条（非对话气泡），可展开摘要 -->
  <div v-if="message.role === 'compaction'" class="py-1">
    <CompactionDivider :part="compactionPart(message)" />
  </div>
  <!-- 后台任务通知：同为全宽分隔条（任务终态后后端插入的 user 行） -->
  <div v-else-if="message.role === 'notification'" class="py-1">
    <TaskNotificationDivider :part="notificationPart(message)" />
  </div>
  <!-- 用户消息：IM 式右侧气泡；助手回合：扁平无头像（对齐 redesign demo） -->
  <div v-else-if="message.role === 'user'" class="flex justify-end">
    <div
      class="max-w-[75%] whitespace-pre-wrap break-words border border-line-soft rounded-xl bg-panel px-3.5 py-2 text-[13px] text-neutral-10 leading-[1.55]"
    >
      <template v-for="(part, i) in message.parts" :key="`t-${i}`">
        {{ part.kind === "text" ? part.text : ""
        }}<span
          v-if="message.isStreaming && i === message.parts.length - 1"
          class="animate-pulse"
          >▌</span
        >
      </template>
    </div>
  </div>
  <div v-else class="space-y-2">
    <template v-for="(part, i) in message.parts" :key="partKey(part, i)">
      <div
        v-if="part.kind === 'text'"
        class="whitespace-pre-wrap break-words text-[13px] text-neutral-10 leading-[1.55]"
      >
        {{ part.text
        }}<span
          v-if="message.isStreaming && i === message.parts.length - 1"
          class="animate-pulse"
          >▌</span
        >
      </div>
      <ThinkingCard v-else-if="part.kind === 'reasoning'" :part="part" />
      <!-- 流式压缩事件落在当前流式消息的 parts 里（通常位于最前部） -->
      <CompactionDivider v-else-if="part.kind === 'compaction'" :part="part" />
      <!-- task 工具（子代理委派）有专属任务卡片，不走通用工具卡片 -->
      <TaskCard
        v-else-if="part.kind === 'tool_call' && part.name === 'task'"
        :part="part"
        :conversation-id="conversationId"
      />
      <ToolCallCard
        v-else-if="part.kind === 'tool_call'"
        :part="part"
        :conversation-id="conversationId"
      />
    </template>
    <!-- 尚无任何内容时的流式占位光标 -->
    <div v-if="message.isStreaming && message.parts.length === 0">
      <span class="animate-pulse">▌</span>
    </div>
  </div>
  <div
    v-if="
      (forkEnabled || revertEnabled) && message.forkCut && !message.isStreaming
    "
    class="flex justify-end"
  >
    <DropdownMenuRoot>
      <DropdownMenuTrigger
        class="h-6 w-6 flex items-center justify-center rounded text-faint control-focus hover:bg-neutral-2"
        title="消息操作"
      >
        <i-lucide-ellipsis class="h-3.5 w-3.5" />
      </DropdownMenuTrigger>
      <DropdownMenuPortal>
        <DropdownMenuContent
          align="end"
          :side-offset="4"
          class="z-50 min-w-[160px] border border-neutral-5 rounded-lg bg-surface p-1 shadow-[var(--oct-shadow)]"
        >
          <DropdownMenuItem
            v-if="forkEnabled"
            class="h-7 flex cursor-pointer items-center gap-2 rounded px-2 text-[12px] text-neutral-10 outline-none data-[highlighted]:bg-neutral-1"
            @select="$emit('fork', message.forkCut!)"
          >
            <i-lucide-git-branch class="h-3.5 w-3.5" />
            {{
              message.role === "user"
                ? "从这条消息前分支"
                : message.forkCut.kind === "message" &&
                    message.forkCut.message_id !== message.id
                  ? "从这一步后分支"
                  : "从这条回复后分支"
            }}
          </DropdownMenuItem>
          <DropdownMenuItem
            v-if="revertEnabled"
            class="h-7 flex cursor-pointer items-center gap-2 rounded px-2 text-[12px] text-neutral-10 outline-none data-[highlighted]:bg-neutral-1"
            @select="$emit('revert', message.forkCut!)"
          >
            <i-lucide-undo-2 class="h-3.5 w-3.5" />
            {{
              message.role === "user"
                ? "回退并编辑"
                : message.forkCut.kind === "message" &&
                    message.forkCut.message_id !== message.id
                  ? "回退到这一步"
                  : "回退到这条回复"
            }}
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenuPortal>
    </DropdownMenuRoot>
  </div>
</template>

<script setup lang="ts">
import {
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuPortal,
  DropdownMenuRoot,
  DropdownMenuTrigger,
} from "reka-ui";
import CompactionDivider from "./CompactionDivider.vue";
import TaskCard from "./TaskCard.vue";
import TaskNotificationDivider from "./TaskNotificationDivider.vue";
import ThinkingCard from "./ThinkingCard.vue";
import ToolCallCard from "./ToolCallCard.vue";
import type {
  CompactionPart,
  DisplayMessage,
  DisplayPart,
  TaskNotificationPart,
} from "@/composables/useChat";

defineProps<{
  message: DisplayMessage;
  /** 「查看原文」（microcompact 副本回查）需要会话作用域。 */
  conversationId: string | null;
  forkEnabled?: boolean;
  revertEnabled?: boolean;
}>();
defineEmits<{
  fork: [cut: import("@/composables/chat/display").ForkCut];
  revert: [cut: import("@/composables/chat/display").ForkCut];
}>();

function partKey(part: DisplayPart, index: number): string {
  return part.kind === "tool_call" ? `c-${part.callId}` : `p-${index}`;
}

/** compaction 角色的消息固定只含一个压缩 part。 */
function compactionPart(message: DisplayMessage): CompactionPart {
  return message.parts[0] as CompactionPart;
}

/** notification 角色的消息固定只含一个通知 part。 */
function notificationPart(message: DisplayMessage): TaskNotificationPart {
  return message.parts[0] as TaskNotificationPart;
}
</script>
