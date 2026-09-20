<template>
  <DialogRoot :open="visible" @update:open="handleUpdateOpen">
    <DialogPortal>
      <DialogOverlay class="fixed inset-0 z-50 bg-overlay" />
      <DialogContent
        class="fixed left-1/2 top-1/2 z-50 max-w-[calc(100vw-72px)] w-[420px] border border-neutral-5 rounded-xl bg-panel p-5 shadow-[var(--oct-shadow)] -translate-x-1/2 -translate-y-1/2"
      >
        <DialogTitle class="text-[16.5px] text-strong font-semibold">
          New Project
        </DialogTitle>
        <p class="mt-1 text-[12px] text-dim">
          创建一个绑定本地工作目录的项目。
        </p>

        <form class="mt-4 flex flex-col gap-3" @submit.prevent="submit">
          <label class="flex flex-col gap-2">
            <span class="text-[12px] text-dim font-medium">Project name</span>
            <input
              v-model="name"
              placeholder="my-project"
              class="h-[30px] w-full control-surface px-3 text-[13px] control-focus"
            />
          </label>
          <label class="flex flex-col gap-2">
            <span class="text-[12px] text-dim font-medium">
              Working directory
            </span>
            <input
              v-model="workingDir"
              placeholder="/home/me/projects/my-project"
              class="h-[30px] w-full control-surface px-3 text-[13px] font-mono control-focus"
            />
          </label>

          <div class="mt-2 flex justify-end gap-2">
            <DialogClose
              class="h-[28px] control-ghost rounded-md px-3 text-[12.5px] control-focus"
            >
              Cancel
            </DialogClose>
            <button
              type="submit"
              class="h-[28px] control-surface rounded-md px-3 text-[12.5px] font-medium control-focus disabled:pointer-events-none disabled:opacity-50"
              :disabled="!canCreate || creating"
            >
              {{ creating ? "Creating…" : "Create" }}
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
import { computed, ref, watch } from "vue";
import { useProjects } from "@/composables/useProjects";

const props = defineProps<{ visible: boolean }>();

const emit = defineEmits<{ close: [] }>();

const { createProject } = useProjects();

const name = ref("");
const workingDir = ref("");
const creating = ref(false);

const canCreate = computed(
  () => name.value.trim().length > 0 && workingDir.value.trim().length > 0,
);

// 每次打开都是空白表单
watch(
  () => props.visible,
  (visible) => {
    if (visible) {
      name.value = "";
      workingDir.value = "";
      creating.value = false;
    }
  },
);

function handleUpdateOpen(open: boolean) {
  if (!open) emit("close");
}

async function submit() {
  if (!canCreate.value || creating.value) return;
  creating.value = true;
  try {
    await createProject(name.value.trim(), workingDir.value.trim());
    emit("close");
  } finally {
    creating.value = false;
  }
}
</script>
