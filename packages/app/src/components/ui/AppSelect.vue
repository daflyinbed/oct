<template>
  <SelectRoot
    :model-value="modelValue"
    :disabled="disabled || options.length === 0"
    @update:model-value="handleUpdate"
  >
    <SelectTrigger
      :title="title"
      class="control-focus inline-flex flex-none items-center gap-1.5 min-w-0 h-[26px] px-2 rounded-md border border-neutral-5 bg-surface text-[12px] transition-colors hover:bg-neutral-3 data-[state=open]:bg-neutral-3 data-[placeholder]:text-faint disabled:pointer-events-none disabled:opacity-50"
      :class="triggerClass"
    >
      <SelectValue :placeholder="placeholder" class="min-w-0 truncate" />
      <i-lucide-chevrons-up-down class="w-3 h-3 flex-none text-faint" />
    </SelectTrigger>

    <SelectPortal>
      <SelectContent
        position="popper"
        :side-offset="6"
        :align="align"
        class="z-50 min-w-[var(--reka-select-trigger-width)] max-h-[min(320px,var(--reka-select-content-available-height))] p-1 rounded-lg border border-line-soft bg-panel shadow-[var(--oct-shadow)]"
      >
        <SelectScrollUpButton
          class="flex h-5 items-center justify-center text-faint"
        >
          <i-lucide-chevron-up class="w-3 h-3" />
        </SelectScrollUpButton>
        <SelectViewport class="overflow-y-auto">
          <SelectItem
            v-for="option in options"
            :key="option.value"
            :value="option.value"
            class="flex items-center gap-2 h-[25px] px-2.5 rounded-[5px] text-[13px] text-neutral-10 outline-none focus-visible:outline-none cursor-pointer data-[highlighted]:bg-neutral-2 data-[disabled]:cursor-default data-[disabled]:opacity-50"
          >
            <SelectItemText class="flex-1 min-w-0 truncate text-left">{{
              option.label
            }}</SelectItemText>
            <SelectItemIndicator class="flex-none">
              <i-lucide-check class="w-3.5 h-3.5 text-neutral-10" />
            </SelectItemIndicator>
          </SelectItem>
        </SelectViewport>
        <SelectScrollDownButton
          class="flex h-5 items-center justify-center text-faint"
        >
          <i-lucide-chevron-down class="w-3 h-3" />
        </SelectScrollDownButton>
      </SelectContent>
    </SelectPortal>
  </SelectRoot>
</template>

<script setup lang="ts">
import type { AcceptableValue } from "reka-ui";
import {
  SelectContent,
  SelectItem,
  SelectItemIndicator,
  SelectItemText,
  SelectPortal,
  SelectRoot,
  SelectScrollDownButton,
  SelectScrollUpButton,
  SelectTrigger,
  SelectValue,
  SelectViewport,
} from "reka-ui";

defineProps<{
  modelValue: string | null;
  options: readonly { value: string; label: string }[];
  placeholder?: string;
  title?: string;
  disabled?: boolean;
  /** 弹出层与触发器的对齐方式 */
  align?: "start" | "center" | "end";
  /** 透传到触发器上的样式（文字颜色、最大宽度等） */
  triggerClass?: string;
}>();

const emit = defineEmits<{
  "update:modelValue": [value: string];
}>();

function handleUpdate(value: AcceptableValue) {
  if (typeof value === "string") emit("update:modelValue", value);
}
</script>
