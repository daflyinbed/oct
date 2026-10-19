<template>
  <div
    v-if="turns.length > 0"
    class="pointer-events-none absolute inset-y-4 left-0 z-10 w-12 flex items-center"
  >
    <nav
      ref="navigation"
      aria-label="用户消息导航"
      class="turn-navigation pointer-events-auto max-h-full w-full overflow-y-auto overscroll-contain"
    >
      <HoverCardRoot
        v-for="(turn, index) in turns"
        :key="turn.id"
        :open="previewId === turn.id"
        :open-delay="100"
        :close-delay="80"
        @update:open="(open) => setPreview(turn.id, open)"
      >
        <HoverCardTrigger as-child>
          <button
            type="button"
            class="turn-indicator h-5 w-full flex items-center pl-3 control-focus"
            :aria-label="`第 ${index + 1} 条用户消息：${turn.question}`"
            :aria-current="index === turns.length - 1 ? 'true' : undefined"
            :aria-describedby="
              previewId === turn.id ? previewElementId : undefined
            "
            @click="navigate(turn.id)"
          >
            <span class="turn-indicator-line" aria-hidden="true" />
          </button>
        </HoverCardTrigger>
        <HoverCardPortal>
          <HoverCardContent
            :id="previewElementId"
            role="tooltip"
            side="right"
            align="start"
            :side-offset="8"
            :collision-padding="12"
            class="z-50 w-[320px] max-w-[calc(100vw-24px)] border border-neutral-5 rounded-xl bg-surface p-4 text-[13px] leading-[1.6] shadow-[var(--oct-shadow)]"
          >
            <p
              class="line-clamp-2 whitespace-pre-wrap break-words text-neutral-10"
            >
              {{ turn.question }}
            </p>
            <p
              class="mt-2.5 line-clamp-3 whitespace-pre-wrap break-words text-dim"
            >
              {{ turn.answer || (turn.streaming ? "正在回复…" : "暂无回复") }}
            </p>
          </HoverCardContent>
        </HoverCardPortal>
      </HoverCardRoot>
    </nav>
  </div>
</template>

<script setup lang="ts">
import {
  HoverCardContent,
  HoverCardPortal,
  HoverCardRoot,
  HoverCardTrigger,
} from "reka-ui";
import { computed, nextTick, ref, useId, watch } from "vue";
import type { DisplayMessage } from "@/composables/useChat";

const props = defineProps<{
  messages: readonly DisplayMessage[];
}>();

const emit = defineEmits<{
  navigate: [messageId: string];
}>();

interface TurnPreview {
  id: string;
  question: string;
  answer: string;
  streaming: boolean;
}

const navigation = ref<HTMLElement | null>(null);
const previewId = ref<string | null>(null);
const previewElementId = `turn-preview-${useId()}`;

function excerpt(text: string): string {
  const trimmed = text.trim();
  return trimmed.length > 360 ? `${trimmed.slice(0, 360)}…` : trimmed;
}

const turns = computed(() => {
  const result: TurnPreview[] = [];
  let current: TurnPreview | undefined;
  for (const message of props.messages) {
    if (message.role === "user") {
      const text = message.parts
        .flatMap((part) => (part.kind === "text" ? [part.text] : []))
        .join("\n");
      const attachments = message.parts.flatMap((part) =>
        part.kind === "image" || part.kind === "file"
          ? [part.filename || (part.kind === "image" ? "图片附件" : "文件附件")]
          : [],
      );
      current = {
        id: message.id,
        question:
          excerpt(text) || excerpt(attachments.join("、")) || "用户消息",
        answer: "",
        streaming: false,
      };
      result.push(current);
    } else if (message.role === "assistant" && current) {
      current.streaming ||= !!message.isStreaming;
      if (!current.answer) {
        // 工具调用可能把一轮拆成多条 assistant 消息，取首次出现的回复正文。
        for (const part of message.parts) {
          if (part.kind === "text" && part.text.trim()) {
            current.answer = excerpt(part.text);
            break;
          }
        }
      }
    }
  }
  return result;
});

function setPreview(messageId: string, open: boolean) {
  if (open) previewId.value = messageId;
  else if (previewId.value === messageId) previewId.value = null;
}

function navigate(messageId: string) {
  previewId.value = null;
  emit("navigate", messageId);
}

watch(
  () => turns.value.at(-1)?.id,
  async () => {
    await nextTick();
    if (navigation.value)
      navigation.value.scrollTop = navigation.value.scrollHeight;
  },
  { immediate: true },
);
</script>

<style scoped>
.turn-navigation {
  scrollbar-width: none;
}

.turn-navigation::-webkit-scrollbar {
  display: none;
}

.turn-indicator-line {
  width: 16px;
  height: 3px;
  border-radius: 999px;
  background: var(--oct-color-faint);
  opacity: 0.65;
  transition:
    width 150ms ease,
    background-color 150ms ease,
    opacity 150ms ease;
}

.turn-indicator[aria-current="true"] .turn-indicator-line {
  background: var(--oct-color-strong);
  opacity: 0.85;
}

.turn-indicator:hover .turn-indicator-line,
.turn-indicator:focus-visible .turn-indicator-line,
.turn-indicator[data-state="open"] .turn-indicator-line {
  width: 32px;
  background: var(--oct-color-strong);
  opacity: 1;
}

@media (prefers-reduced-motion: reduce) {
  .turn-indicator-line {
    transition: none;
  }
}
</style>
