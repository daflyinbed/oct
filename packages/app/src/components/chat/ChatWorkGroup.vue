<template>
  <div
    data-chat-work
    :data-work-state="work.state"
    @reveal-chat-work="open = true"
  >
    <CollapsibleRoot v-if="work.history.length" v-model:open="open">
      <CollapsibleTrigger as-child>
        <button
          type="button"
          data-testid="chat-work-toggle"
          class="group max-w-full inline-flex items-center gap-1.5 rounded py-1 text-sm text-dim control-focus transition-colors hover:text-neutral-10"
          :aria-label="`${open ? '收起' : '展开'}工作过程`"
        >
          <i-lucide-loader-circle
            v-if="work.state === 'running'"
            class="h-3.5 w-3.5 flex-none animate-spin text-accent-7"
          />
          <i-lucide-circle-alert
            v-else-if="work.state === 'failed'"
            class="h-3.5 w-3.5 flex-none text-danger-10"
          />
          <i-lucide-circle-stop
            v-else-if="work.state === 'interrupted'"
            class="h-3.5 w-3.5 flex-none text-warning-10"
          />
          <span>{{ statusText }}</span>
          <i-lucide-chevron-right
            class="h-3.5 w-3.5 flex-none text-faint transition-transform duration-150 motion-reduce:transition-none"
            :class="{ 'rotate-90': open }"
          />
        </button>
      </CollapsibleTrigger>
      <!-- 保留卡片实例，反复查看过程时不丢失思考/工具的手动展开状态。 -->
      <CollapsibleContent v-show="open" force-mount>
        <div
          data-chat-work-history
          class="mb-2 ml-1 mt-1 border-l border-line-soft py-1 pl-3 space-y-2"
        >
          <div
            v-for="message in work.history"
            :key="message.id"
            :data-message-id="
              message.id === work.answer?.id ? undefined : message.id
            "
          >
            <ChatMessage
              :message="message"
              :conversation-id="conversationId"
              :fork-enabled="forkEnabled"
              process
              @fork="(cut) => $emit('fork', cut)"
            />
          </div>
        </div>
      </CollapsibleContent>
    </CollapsibleRoot>
    <div
      v-if="work.answer"
      :data-message-id="work.answer.id"
      :class="{ 'mt-2': work.history.length }"
      data-chat-answer
    >
      <ChatMessage
        :message="work.answer"
        :conversation-id="conversationId"
        :fork-enabled="forkEnabled"
        @fork="(cut) => $emit('fork', cut)"
      />
    </div>
    <div
      v-for="message in work.following"
      :key="message.id"
      :data-message-id="message.id"
      class="mt-2"
    >
      <ChatMessage
        :message="message"
        :conversation-id="conversationId"
        :fork-enabled="forkEnabled"
        @fork="(cut) => $emit('fork', cut)"
      />
    </div>
  </div>
</template>

<script setup lang="ts">
import { useIntervalFn } from "@vueuse/core";
import {
  CollapsibleContent,
  CollapsibleRoot,
  CollapsibleTrigger,
} from "reka-ui";
import { computed, ref, watch } from "vue";
import ChatMessage from "./ChatMessage.vue";
import { formatWorkDuration } from "./chatWork";
import type { ChatWork } from "./chatWork";
import type { ForkCut } from "@/composables/chat/display";

const props = defineProps<{
  work: ChatWork;
  conversationId: string | null;
  forkEnabled?: boolean;
}>();

defineEmits<{ fork: [cut: ForkCut] }>();

const open = ref(props.work.defaultOpen);
const now = ref(Date.now());
const { pause, resume } = useIntervalFn(
  () => {
    now.value = Date.now();
  },
  1000,
  { immediate: false },
);

watch(
  () => props.work.state,
  (state) => {
    if (state === "running") resume();
    else pause();
  },
  { immediate: true },
);

// 流式过程结束后收起；同一状态里的增量不覆盖用户手动展开/收起的选择。
watch([() => props.work.state, () => props.work.defaultOpen], ([, value]) => {
  open.value = value;
});

const statusText = computed(() => {
  const durationMs =
    props.work.state === "running" && props.work.startedAt != null
      ? (props.work.durationMs ?? 0) +
        Math.max(0, now.value - props.work.startedAt)
      : props.work.durationMs;
  const duration = durationMs == null ? "" : formatWorkDuration(durationMs);
  switch (props.work.state) {
    case "running":
      return duration ? `已工作${duration}` : "工作中";
    case "interrupted":
      return "已停止";
    case "failed":
      return "执行失败";
    default:
      return duration ? `已工作${duration}` : "已工作（耗时未知）";
  }
});
</script>
