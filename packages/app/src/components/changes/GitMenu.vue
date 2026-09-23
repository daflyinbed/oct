<template>
  <DropdownMenuRoot>
    <DropdownMenuTrigger
      class="control-ghost rounded px-1.5 py-1 control-focus disabled:opacity-40"
      :disabled="disabled"
      :aria-label="label"
      :title="label"
      ><slot><i-lucide-chevron-down class="h-3 w-3" /></slot
    ></DropdownMenuTrigger>
    <DropdownMenuPortal>
      <DropdownMenuContent
        :side="side ?? 'bottom'"
        align="end"
        :side-offset="5"
        class="z-50 min-w-48 border border-line-soft rounded-lg bg-surface p-1 text-[12px] shadow-[var(--oct-shadow)]"
      >
        <template v-for="(group, index) in groups" :key="index">
          <DropdownMenuSeparator v-if="index" class="my-1 h-px bg-line-soft" />
          <template v-for="item in group" :key="item.id">
            <DropdownMenuSub v-if="item.children?.length">
              <DropdownMenuSubTrigger
                class="flex cursor-pointer justify-between gap-4 rounded px-2 py-1.5 outline-none data-[highlighted]:bg-neutral-2"
                >{{ item.label }} <span>›</span></DropdownMenuSubTrigger
              >
              <DropdownMenuPortal
                ><DropdownMenuSubContent
                  class="z-50 min-w-36 border border-line-soft rounded-lg bg-surface p-1 text-[12px] shadow-[var(--oct-shadow)]"
                >
                  <DropdownMenuItem
                    v-for="child in item.children"
                    :key="child.id"
                    class="cursor-pointer rounded px-2 py-1.5 outline-none data-[highlighted]:bg-neutral-2"
                    @select="emit('select', child.id)"
                    >{{ child.label }}</DropdownMenuItem
                  >
                </DropdownMenuSubContent></DropdownMenuPortal
              >
            </DropdownMenuSub>
            <DropdownMenuItem
              v-else
              :disabled="item.disabled"
              class="flex cursor-pointer justify-between gap-4 rounded px-2 py-1.5 outline-none data-[highlighted]:bg-neutral-2 data-[disabled]:opacity-40"
              @select="emit('select', item.id)"
              >{{ item.label
              }}<span v-if="item.checked">✓</span></DropdownMenuItem
            >
          </template>
        </template>
      </DropdownMenuContent>
    </DropdownMenuPortal>
  </DropdownMenuRoot>
</template>
<script setup lang="ts">
import {
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuPortal,
  DropdownMenuRoot,
  DropdownMenuSeparator,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from "reka-ui";

export interface MenuItem {
  id: string;
  label: string;
  disabled?: boolean;
  checked?: boolean;
  children?: MenuItem[];
}
defineProps<{
  label: string;
  disabled?: boolean;
  groups: MenuItem[][];
  side?: "top" | "bottom";
}>();
const emit = defineEmits<{ select: [id: string] }>();
</script>
