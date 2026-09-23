<template>
  <DialogRoot :open="true" @update:open="handleUpdateOpen">
    <DialogPortal>
      <DialogOverlay class="fixed inset-0 z-50 bg-overlay" />
      <DialogContent
        class="fixed left-1/2 top-1/2 z-50 max-w-[calc(100vw-48px)] w-[440px] flex flex-col overflow-hidden border border-neutral-5 rounded-xl bg-panel shadow-[var(--oct-shadow)] -translate-x-1/2 -translate-y-1/2"
      >
        <div
          class="flex items-center gap-2 border-b border-line-soft px-4 py-3"
        >
          <i-lucide-terminal class="h-4 w-4 flex-none text-dim" />
          <DialogTitle
            class="min-w-0 flex-1 truncate text-[13px] font-semibold"
          >
            /{{ promptName }}
          </DialogTitle>
          <span class="flex-none text-[11.5px] text-faint">{{
            serverName
          }}</span>
        </div>

        <form class="flex flex-col gap-3 px-4 py-3.5" @submit.prevent="submit">
          <DialogDescription
            v-if="promptDescription"
            class="text-[12px] text-dim"
          >
            {{ promptDescription }}
          </DialogDescription>
          <DialogDescription v-else class="sr-only">
            MCP prompt {{ promptName }}
          </DialogDescription>
          <label
            v-for="argument in prompt.arguments"
            :key="argument.name"
            class="flex flex-col gap-1"
          >
            <span class="flex items-center gap-1.5 text-[12px] text-neutral-10">
              <span class="font-mono">{{ argument.name }}</span>
              <span v-if="argument.required" class="text-danger-10">*</span>
              <span v-if="!argument.required" class="text-[10.5px] text-faint"
                >(可选)</span
              >
            </span>
            <span v-if="argument.description" class="text-[11px] text-faint">{{
              argument.description
            }}</span>
            <input
              v-model="values[argument.name]"
              :placeholder="argument.description ?? argument.name"
              class="h-8 border border-line-soft rounded-md bg-surface px-2.5 text-[12.5px] text-neutral-10 outline-none transition-colors focus:border-accent-6"
              @keydown.escape.stop
            />
          </label>
          <p v-if="formError" class="text-[11.5px] text-danger-10">
            {{ formError }}
          </p>
          <div class="flex items-center justify-end gap-1.5 pt-0.5">
            <button
              type="button"
              class="h-7 control-surface rounded-md px-2.5 text-[12px] font-medium control-focus"
              @click="$emit('cancel')"
            >
              取消
            </button>
            <button
              type="submit"
              :disabled="submitting"
              class="h-7 inline-flex items-center gap-1.5 rounded-md bg-accent-7 px-2.5 text-[12px] text-white font-medium control-focus transition-[filter] disabled:opacity-50 hover:brightness-110"
            >
              <i-lucide-loader-circle
                v-if="submitting"
                class="h-3 w-3 animate-spin"
              />
              渲染
            </button>
          </div>
        </form>
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
import { computed, reactive, ref } from "vue";
import type { components } from "@/api/schema";

type McpPromptSummary = components["schemas"]["McpPromptSummary"];

const props = defineProps<{
  serverName: string;
  prompt: McpPromptSummary;
  submitting: boolean;
}>();

const emit = defineEmits<{
  /** 提交：values 为参数名→字符串值（可选参数空值被剔除）。 */
  submit: [values: Record<string, string>];
  cancel: [];
}>();

const promptName = computed(() => props.prompt.name);
const promptDescription = computed(() => props.prompt.description);

const values = reactive<Record<string, string>>(
  Object.fromEntries(props.prompt.arguments.map((a) => [a.name, ""])),
);
const formError = ref<string | null>(null);

function handleUpdateOpen(open: boolean) {
  // Escape / 遮罩点击关 Dialog 的语义即取消。
  if (!open) emit("cancel");
}

function submit() {
  const missing = props.prompt.arguments.filter(
    (argument) => argument.required && !(values[argument.name] ?? "").trim(),
  );
  if (missing.length > 0) {
    formError.value = `必填参数缺失：${missing.map((a) => a.name).join("、")}`;
    return;
  }
  formError.value = null;
  const filled: Record<string, string> = {};
  for (const argument of props.prompt.arguments) {
    const value = (values[argument.name] ?? "").trim();
    if (value) filled[argument.name] = value;
  }
  emit("submit", filled);
}
</script>
