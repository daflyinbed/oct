<template>
  <DialogRoot :open="visible" @update:open="handleUpdateOpen">
    <DialogPortal>
      <DialogOverlay class="fixed inset-0 z-50 bg-overlay" />
      <DialogContent
        class="fixed left-1/2 top-1/2 z-50 max-h-[85vh] max-w-[calc(100vw-72px)] w-[420px] overflow-y-auto border border-neutral-5 rounded-xl bg-panel p-5 shadow-[var(--oct-shadow)] -translate-x-1/2 -translate-y-1/2"
      >
        <DialogTitle class="text-xl text-strong font-semibold">
          {{ isEdit ? "Project Settings" : "New Project" }}
        </DialogTitle>
        <DialogDescription class="mt-1 text-sm text-dim">
          {{
            isEdit
              ? "编辑项目的名称与工作目录。"
              : "创建一个绑定本地工作目录的项目。"
          }}
        </DialogDescription>

        <form class="mt-4 flex flex-col gap-3" @submit.prevent="submit">
          <label class="flex flex-col gap-2">
            <span class="text-sm text-dim font-medium">Project name</span>
            <input
              v-model="name"
              placeholder="my-project"
              class="h-[30px] w-full control-surface px-3 text-base control-focus"
            />
          </label>
          <div class="flex flex-col gap-2">
            <span class="text-sm text-dim font-medium">
              Working directory
            </span>
            <DirectoryPicker
              v-model="workingDir"
              label="Working directory"
              placeholder="/home/me/projects/my-project"
              @select="suggestName"
            />
          </div>

          <div class="flex flex-col gap-2">
            <span class="text-sm text-dim font-medium">
              Additional directories
            </span>
            <p class="text-xs text-faint">
              agent 只能通过绝对路径访问这些目录；相对路径始终锚定主工作目录。
            </p>
            <div
              v-for="(dir, index) in additionalDirs"
              :key="index"
              class="flex items-center gap-1.5"
            >
              <DirectoryPicker
                v-model="additionalDirs[index]!"
                :label="`Additional directory ${index + 1}`"
                placeholder="/home/me/another-repo"
                class="min-w-0 flex-1"
              />
              <button
                type="button"
                class="h-[30px] w-[30px] flex-none control-ghost rounded-md text-sm control-focus"
                :title="dir.trim() ? `移除 ${dir}` : '移除此目录'"
                @click="additionalDirs.splice(index, 1)"
              >
                ✕
              </button>
            </div>
            <button
              type="button"
              class="h-[28px] self-start control-ghost rounded-md px-2.5 text-sm control-focus"
              @click="additionalDirs.push('')"
            >
              + 添加目录
            </button>
          </div>

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
              {{
                submitting
                  ? isEdit
                    ? "Saving…"
                    : "Creating…"
                  : isEdit
                    ? "Save"
                    : "Create"
              }}
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
  DialogDescription,
  DialogOverlay,
  DialogPortal,
  DialogRoot,
  DialogTitle,
} from "reka-ui";
import { computed, ref, watch } from "vue";
import { useProjects } from "@/composables/useProjects";
import DirectoryPicker from "@/components/ui/DirectoryPicker.vue";
import type { components } from "@/api/schema";

type Project = components["schemas"]["Project"];

const props = defineProps<{
  visible: boolean;
  /** null = 新建模式；非 null = 编辑该项目（每次打开时取其字段预填）。 */
  project: Project | null;
}>();

const emit = defineEmits<{ close: [] }>();

const { createProject, updateProject } = useProjects();

const name = ref("");
const workingDir = ref("");
const additionalDirs = ref<string[]>([]);
const submitting = ref(false);
const submitError = ref<string | null>(null);

const isEdit = computed(() => props.project !== null);

const canSubmit = computed(
  () => name.value.trim().length > 0 && workingDir.value.trim().length > 0,
);

// 每次打开重置表单：新建为空白，编辑预填当前项目字段。
watch(
  () => props.visible,
  (visible) => {
    if (visible) {
      name.value = props.project?.name ?? "";
      workingDir.value = props.project?.working_dir ?? "";
      additionalDirs.value = [...(props.project?.additional_dirs ?? [])];
      submitting.value = false;
      submitError.value = null;
    }
  },
);

function handleUpdateOpen(open: boolean) {
  if (!open) emit("close");
}

function suggestName(path: string) {
  if (!isEdit.value && !name.value.trim()) {
    name.value = path.split("/").filter(Boolean).at(-1) ?? "";
  }
}

async function submit() {
  if (!canSubmit.value || submitting.value) return;
  const dirs = additionalDirs.value.map((dir) => dir.trim()).filter(Boolean);
  submitting.value = true;
  submitError.value = null;
  try {
    // 失败时保持对话框打开并提示：网络错/相对路径 400 等，静默关闭会让
    // 用户以为已保存。
    const ok = props.project
      ? await updateProject(props.project.id, {
          name: name.value.trim(),
          working_dir: workingDir.value.trim(),
          additional_dirs: dirs,
        })
      : (await createProject(
          name.value.trim(),
          workingDir.value.trim(),
          dirs.length > 0 ? dirs : undefined,
        )) !== null;
    if (ok) {
      emit("close");
    } else {
      submitError.value = props.project
        ? "保存失败，请检查后重试（附加目录必须是绝对路径）。"
        : "创建失败，请检查后重试（附加目录必须是绝对路径）。";
    }
  } finally {
    submitting.value = false;
  }
}
</script>
