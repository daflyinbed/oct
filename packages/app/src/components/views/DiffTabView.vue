<template>
  <section class="workspace-panel min-w-0 flex-1 bg-background">
    <header
      class="h-9 flex flex-none items-center gap-2 border-b border-line-soft px-4 text-[12.5px] font-semibold"
    >
      <i-lucide-git-compare class="h-3.5 w-3.5 text-dim" /><span
        class="min-w-0 flex-1 truncate"
        >{{ title || "Working Tree" }}</span
      >
      <button
        class="control-ghost rounded p-1"
        :disabled="loading"
        title="刷新 diff"
        @click="load"
      >
        <i-lucide-refresh-cw class="h-3 w-3" />
      </button>
    </header>
    <p v-if="error" role="alert" class="m-3 whitespace-pre-wrap text-danger-10">
      {{ error }}
    </p>
    <p v-else-if="loading" class="p-4 text-dim">加载 diff…</p>
    <p v-else-if="!patch" class="p-4 text-dim">
      {{ projectId ? "没有差异" : "请先选择项目" }}
    </p>
    <div v-else class="workspace-panel-body overflow-auto py-2">
      <pre
        class="min-w-max text-[12px] leading-5"
      ><code><span v-for="(line, index) in patch.split('\n')" :key="index" class="block px-4" :class="lineClass(line)">{{ line || ' ' }}</span></code></pre>
    </div>
  </section>
</template>
<script setup lang="ts">
import { onScopeDispose, ref, watch } from "vue";
import client from "@/api/client";
import { gitChanged, gitError } from "@/composables/useGit";
import type { DiffSelection } from "@/composables/useGit";

const props = defineProps<{
  title?: string;
  projectId: string | null;
  selection?: DiffSelection;
}>();
const patch = ref("");
const error = ref("");
const loading = ref(false);
let generation = 0;
let controller: AbortController | undefined;
async function load() {
  const version = ++generation;
  controller?.abort();
  patch.value = "";
  error.value = "";
  loading.value = false;
  if (!props.projectId) return;
  loading.value = true;
  controller = new AbortController();
  try {
    const result = await client.GET("/api/projects/{projectId}/git/diff", {
      params: {
        path: { projectId: props.projectId },
        query: props.selection ?? {},
      },
      signal: controller.signal,
    });
    if (version !== generation) return;
    if (!result.data) throw new Error(gitError(result.error));
    patch.value = result.data.patch;
  } catch (error_) {
    if (version === generation) error.value = gitError(error_);
  } finally {
    if (version === generation) loading.value = false;
  }
}
watch([() => props.projectId, () => props.selection, gitChanged], load, {
  immediate: true,
});
onScopeDispose(() => {
  generation++;
  controller?.abort();
});
function lineClass(line: string) {
  if (line.startsWith("diff ") || line.startsWith("@@"))
    return "bg-neutral-2 text-accent-10";
  if (line.startsWith("+")) return "bg-success-1 text-success-10";
  if (line.startsWith("-")) return "bg-danger-1 text-danger-10";
  return "text-dim";
}
</script>
