<template>
  <!-- 消息列表 + 滚动容器：主面板与子代理 panel 共用的展示链路
       （ChatMessage / 思考块 / 工具卡片 / 压缩分隔条），各自传入
       实例化的 messages 与会话作用域 -->
  <div class="relative min-h-0 flex flex-1">
    <div
      ref="scrollContainer"
      class="workspace-panel-body min-w-0"
      @scroll="onScroll"
      @scrollend="onScrollEnd"
    >
      <div
        ref="messageContent"
        class="mx-auto max-w-[1000px] pb-3 pl-12 pr-7 pt-[18px]"
      >
        <!-- 普通消息与工作段保持 18px 回合间距；段内工具批次的延续维持 8px。 -->
        <div
          v-for="(unit, index) in renderUnits"
          :key="unit.key"
          :class="index > 0 ? 'mt-[18px]' : undefined"
          :data-message-id="
            unit.kind === 'message' ? unit.message.id : undefined
          "
        >
          <ChatWorkGroup
            v-if="unit.kind === 'work'"
            :work="unit"
            :conversation-id="conversationId"
            :fork-enabled="forkEnabled"
            @fork="(cut) => $emit('fork', cut)"
          />
          <ChatMessage
            v-else
            :message="unit.message"
            :conversation-id="conversationId"
            :fork-enabled="forkEnabled"
            :revert-enabled="revertEnabled"
            @fork="(cut) => $emit('fork', cut)"
            @revert="(cut) => $emit('revert', cut)"
          />
        </div>
        <div v-if="messages.length === 0" class="pt-20 text-center text-faint">
          <p class="mb-1.5 text-lg text-dim font-medium">
            {{ emptyTitle }}
          </p>
          <p class="text-base">{{ emptyHint }}</p>
        </div>
      </div>
    </div>
    <ChatTurnNavigator
      :key="conversationId ?? ''"
      :messages="messages"
      :active-id="activeTurnId"
      @navigate="scrollToMessage"
    />
  </div>
</template>

<script setup lang="ts">
import { useResizeObserver } from "@vueuse/core";
import { computed, nextTick, onMounted, ref, watch } from "vue";
import ChatMessage from "./ChatMessage.vue";
import ChatTurnNavigator from "./ChatTurnNavigator.vue";
import ChatWorkGroup from "./ChatWorkGroup.vue";
import { buildChatRenderUnits } from "./chatWork";
import type { DisplayMessage } from "@/composables/useChat";

const props = withDefaults(
  defineProps<{
    messages: readonly DisplayMessage[];
    /** 「查看原文」（microcompact 副本回查）与子代理任务卡片的会话作用域。 */
    conversationId: string | null;
    emptyTitle?: string;
    emptyHint?: string;
    forkEnabled?: boolean;
    revertEnabled?: boolean;
    interrupted?: boolean;
  }>(),
  {
    emptyTitle: "Start a conversation",
    emptyHint: "Select a chat or create a new one",
  },
);

defineEmits<{
  fork: [cut: import("@/composables/chat/display").ForkCut];
  revert: [cut: import("@/composables/chat/display").ForkCut];
}>();

const scrollContainer = ref<HTMLElement | null>(null);
const messageContent = ref<HTMLElement | null>(null);
const renderUnits = computed(() =>
  buildChatRenderUnits(props.messages, {
    interrupted: props.interrupted,
  }),
);
const followingLatest = ref(true);
const activeTurnId = ref<string | null>(null);
let scrollingToMessage = false;

function onScroll() {
  updateActiveTurn();
  if (scrollingToMessage) return;
  const container = scrollContainer.value;
  if (container) {
    followingLatest.value =
      container.scrollHeight - container.scrollTop - container.clientHeight <
      24;
  }
}

// scroll-spy（文档站目录式）：视口顶线以上最后一条用户消息为当前轮次。
// 顶线取容器顶 + 24px——比 scrollToMessage 的 18px 定位边距略宽，留亚像素容差，
// 保证点击指示条定位后高亮落在被点击的那条；贴底跟随最新回复时直接高亮最后一条。
function updateActiveTurn() {
  const container = scrollContainer.value;
  if (!container) return;
  const turnIds = props.messages
    .filter((message) => message.role === "user")
    .map((message) => message.id);
  if (turnIds.length === 0) {
    activeTurnId.value = null;
    return;
  }
  if (
    container.scrollHeight - container.scrollTop - container.clientHeight <
    24
  ) {
    activeTurnId.value = turnIds.at(-1)!;
    return;
  }
  const tops = new Map<string, number>();
  for (const element of container.querySelectorAll<HTMLElement>(
    "[data-message-id]",
  )) {
    tops.set(element.dataset.messageId!, element.getBoundingClientRect().top);
  }
  const threshold = container.getBoundingClientRect().top + 24;
  let active = turnIds[0]!;
  for (const id of turnIds) {
    const top = tops.get(id);
    if (top === undefined) continue;
    if (top <= threshold) active = id;
    else break;
  }
  activeTurnId.value = active;
}

function onScrollEnd() {
  scrollingToMessage = false;
  onScroll();
}

function scrollToMessage(messageId: string) {
  const container = scrollContainer.value;
  const target = container?.querySelector<HTMLElement>(
    `[data-message-id="${CSS.escape(messageId)}"]`,
  );
  if (!container || !target) return;
  followingLatest.value = false;
  const top = Math.max(
    0,
    Math.min(
      container.scrollHeight - container.clientHeight,
      container.scrollTop +
        target.getBoundingClientRect().top -
        container.getBoundingClientRect().top -
        18,
    ),
  );
  scrollingToMessage = Math.abs(container.scrollTop - top) > 1;
  container.scrollTo({
    top,
    behavior: window.matchMedia("(prefers-reduced-motion: reduce)").matches
      ? "instant"
      : "smooth",
  });
}

async function scrollToBottom() {
  await nextTick();
  if (followingLatest.value && scrollContainer.value) {
    scrollContainer.value.scrollTop = scrollContainer.value.scrollHeight;
  }
}

watch(
  () => props.conversationId,
  () => {
    scrollingToMessage = false;
    followingLatest.value = true;
    void scrollToBottom().then(updateActiveTurn);
  },
);

watch(
  () => props.messages.filter((message) => message.role === "user").length,
  (count, previousCount) => {
    // 发送后历史刷新会把临时 id 换成持久化 id；只有新增提问才恢复跟随。
    if (count <= previousCount) return;
    scrollingToMessage = false;
    followingLatest.value = true;
    scrollToBottom();
  },
);

watch(
  // 终态历史会以持久化 id 替换 pending/stream id，消息数可能完全不变；
  // 用 id 序列同时覆盖增删与同长度替换，避免 activeTurnId 悬空。
  () =>
    props.messages
      .map(
        (message) =>
          `${message.id}:${message.isStreaming}:${message.work?.state}`,
      )
      .join("\n"),
  async () => {
    await scrollToBottom();
    updateActiveTurn();
  },
);

onMounted(updateActiveTurn);
// 展开/收起工作过程会改变轮次位置，导航高亮随布局重新计算。
useResizeObserver(messageContent, updateActiveTurn);

// 消息内容增长（文本/思考/工具参数/实时输出）时跟随滚动到底部；回看历史时暂停跟随。
watch(
  () => {
    const last = props.messages.at(-1);
    if (!last) return 0;
    let size = 0;
    for (const part of last.parts) {
      if (part.kind === "compaction") {
        size += part.summary.length;
      } else if (part.kind === "task_notification") {
        size += part.tasks.length;
      } else if (part.kind === "text" || part.kind === "reasoning") {
        size += part.text.length;
      } else if (part.kind === "image" || part.kind === "file") {
        // 附件缩略图/文件链接有固定高度，不参与文本增长估算。
        size += 0;
      } else {
        size += part.arguments.length + part.liveOutput.droppedChars;
        for (const seg of part.liveOutput.segments) {
          size += seg.text.length;
        }
      }
    }
    return size;
  },
  () => scrollToBottom(),
);
</script>
