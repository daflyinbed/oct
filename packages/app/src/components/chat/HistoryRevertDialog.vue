<template>
  <DialogRoot
    :open="true"
    @update:open="
      (open) => {
        if (!open && !submitting) $emit('cancel');
      }
    "
  >
    <DialogPortal>
      <DialogOverlay class="fixed inset-0 z-50 bg-overlay" />
      <DialogContent
        class="fixed left-1/2 top-1/2 z-50 max-w-[calc(100vw-48px)] w-[440px] border border-neutral-5 rounded-xl bg-panel p-4 shadow-[var(--oct-shadow)] -translate-x-1/2 -translate-y-1/2"
      >
        <DialogTitle class="mb-3 text-lg font-semibold">回退对话</DialogTitle>
        <DialogDescription class="text-base text-dim leading-relaxed">
          {{
            beforeUser
              ? "退回这条用户消息之前，原文将放回输入框。"
              : "保留到所选回复或工具步骤结束的位置。"
          }}
          将从当前会话隐藏
          {{ removedCount }} 条消息，并自动保存一份完整的只读备份。
        </DialogDescription>
        <p
          v-if="preview"
          class="mt-3 max-h-24 overflow-auto whitespace-pre-wrap border border-line-soft rounded-lg p-2 text-sm text-dim"
        >
          {{ preview }}
        </p>
        <p class="mt-3 text-sm text-warning-10">
          仅回退对话；工作区文件保持当前状态。
        </p>
        <p v-if="error" role="alert" class="mt-3 text-sm text-danger-10">
          {{ error }}
        </p>
        <div class="mt-4 flex justify-end gap-2">
          <button
            type="button"
            :disabled="submitting"
            class="h-7 control-surface rounded-md px-3 text-sm control-focus"
            @click="$emit('cancel')"
          >
            取消
          </button>
          <button
            type="button"
            :disabled="submitting"
            class="h-7 rounded-md bg-accent-7 px-3 text-sm text-white control-focus disabled:opacity-50"
            @click="$emit('confirm')"
          >
            {{ submitting ? "回退中…" : "确认回退" }}
          </button>
        </div>
      </DialogContent>
    </DialogPortal>
  </DialogRoot>
</template>

<script setup lang="ts">
import {
  DialogContent,
  DialogDescription,
  DialogOverlay,
  DialogPortal,
  DialogRoot,
  DialogTitle,
} from "reka-ui";

defineProps<{
  beforeUser: boolean;
  removedCount: number;
  preview: string;
  submitting: boolean;
  error: string | null;
}>();
defineEmits<{ confirm: []; cancel: [] }>();
</script>
