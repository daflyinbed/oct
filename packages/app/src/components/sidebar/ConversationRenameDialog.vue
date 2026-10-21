<template>
  <DialogRoot :open="visible" @update:open="handleUpdateOpen">
    <DialogPortal>
      <DialogOverlay class="fixed inset-0 z-50 bg-overlay" />
      <DialogContent
        class="fixed left-1/2 top-1/2 z-50 max-w-[calc(100vw-72px)] w-[360px] border border-neutral-5 rounded-xl bg-panel p-5 shadow-[var(--oct-shadow)] -translate-x-1/2 -translate-y-1/2"
      >
        <DialogTitle class="text-xl text-strong font-semibold">
          Rename Conversation
        </DialogTitle>
        <p class="mt-1 text-sm text-dim">
          手动命名的标题不会再被自动生成的标题覆盖。
        </p>

        <form class="mt-4 flex flex-col gap-3" @submit.prevent="submit">
          <label class="flex flex-col gap-2">
            <span class="text-sm text-dim font-medium">Title</span>
            <input
              ref="inputEl"
              v-model="title"
              placeholder="New Chat"
              class="h-[30px] w-full control-surface px-3 text-base control-focus"
            />
          </label>

          <p v-if="submitError" class="text-sm text-danger-10" role="alert">
            {{ submitError }}
          </p>

          <div class="mt-2 flex justify-end gap-2">
            <DialogClose
              class="h-[28px] control-ghost rounded-md px-3 text-base control-focus"
            >
              Cancel
            </DialogClose>
            <button
              type="submit"
              class="h-[28px] control-surface rounded-md px-3 text-base font-medium control-focus disabled:pointer-events-none disabled:opacity-50"
              :disabled="!canSubmit || submitting"
            >
              {{ submitting ? "Saving…" : "Save" }}
            </button>
          </div>
        </form>
      </DialogContent>
    </DialogPortal>
  </DialogRoot>
</template>

<script setup lang="ts">
import {
  DialogClose,
  DialogContent,
  DialogOverlay,
  DialogPortal,
  DialogRoot,
  DialogTitle,
} from "reka-ui";
import { computed, nextTick, ref, watch } from "vue";
import { useProjects } from "@/composables/useProjects";
import type { components } from "@/api/schema";

type Conversation = components["schemas"]["Conversation"];

const props = defineProps<{
  visible: boolean;
  /** 打开时取其标题预填；null 时对话框不可见，提交前有兜底。 */
  conversation: Conversation | null;
}>();

const emit = defineEmits<{ close: [] }>();

const { renameConversation } = useProjects();

const title = ref("");
const submitting = ref(false);
const submitError = ref<string | null>(null);
const inputEl = ref<HTMLInputElement | null>(null);

const canSubmit = computed(() => title.value.trim().length > 0);

// 每次打开预填当前标题并全选——改名最常见意图是整体替换，直接输入即可覆盖。
watch(
  () => props.visible,
  (visible) => {
    if (visible) {
      title.value = props.conversation?.title ?? "";
      submitting.value = false;
      submitError.value = null;
      nextTick(() => {
        inputEl.value?.focus();
        inputEl.value?.select();
      });
    }
  },
);

function handleUpdateOpen(open: boolean) {
  // Esc / 点击遮罩 / Cancel 都走这里：只关闭，不发请求。
  if (!open) emit("close");
}

async function submit() {
  if (!canSubmit.value || submitting.value) return;
  const conversation = props.conversation;
  if (!conversation) return;
  submitting.value = true;
  submitError.value = null;
  try {
    // 失败时保持对话框打开并提示：静默关闭会让用户以为已保存。
    const ok = await renameConversation(
      conversation.project_id,
      conversation.id,
      title.value,
    );
    if (ok) {
      emit("close");
    } else {
      submitError.value = "重命名失败，请稍后重试。";
    }
  } finally {
    submitting.value = false;
  }
}
</script>
