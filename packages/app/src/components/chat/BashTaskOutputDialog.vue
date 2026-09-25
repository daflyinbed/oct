<template>
  <!-- 后台任务输出查看器：懒加载（挂载即取尾部 64KiB），「加载更早」用
       next_offset 向上翻页（字节偏移，从文件头方向推进）。输出是有损
       UTF-8 文本，多字节字符被切断的边界直接显示替换符 -->
  <DialogRoot :open="true" @update:open="handleUpdateOpen">
    <DialogPortal>
      <DialogOverlay class="fixed inset-0 z-50 bg-overlay" />
      <DialogContent
        class="fixed left-1/2 top-1/2 z-50 max-h-[calc(100vh-96px)] max-w-[calc(100vw-48px)] w-[720px] flex flex-col overflow-hidden border border-neutral-5 rounded-xl bg-panel shadow-[var(--oct-shadow)] -translate-x-1/2 -translate-y-1/2"
      >
        <div
          class="flex items-center gap-2 border-b border-line-soft px-4 py-3"
        >
          <i-lucide-terminal class="h-4 w-4 flex-none text-dim" />
          <DialogTitle
            class="min-w-0 flex-1 truncate text-[13px] font-semibold"
          >
            <span class="font-mono" :title="command">{{ command }}</span>
          </DialogTitle>
          <span class="flex-none text-[11px] text-faint font-mono">{{
            taskId.slice(0, 8)
          }}</span>
          <button
            type="button"
            class="h-6 w-6 inline-flex flex-none items-center justify-center rounded-md text-dim control-focus transition-colors hover:bg-neutral-2 hover:text-neutral-10"
            title="关闭"
            @click="$emit('close')"
          >
            <i-lucide-x class="h-3.5 w-3.5" />
          </button>
        </div>
        <DialogDescription class="sr-only">
          后台任务的输出文件内容
        </DialogDescription>

        <div class="min-h-0 flex flex-col px-4 py-3">
          <!-- 向上分页：next_offset 存在说明文件里还有更早的内容 -->
          <div
            v-if="nextOffset !== null"
            class="mb-1.5 flex items-center gap-2"
          >
            <button
              type="button"
              class="h-6 control-surface rounded-md px-2 text-[11.5px] control-focus disabled:opacity-50"
              :class="earlierFailed ? 'text-danger-10' : ''"
              :disabled="loadingEarlier"
              @click="loadEarlier"
            >
              {{
                loadingEarlier
                  ? "加载中…"
                  : earlierFailed
                    ? "加载失败，重试"
                    : `加载更早（已载 ${formatBackgroundBytes(loadedBytes)} / 共 ${formatBackgroundBytes(totalBytes)}）`
              }}
            </button>
            <span class="text-[11px] text-faint">输出从尾部开始展示</span>
          </div>

          <div
            v-if="loading && output === null"
            class="flex items-center justify-center gap-2 py-8 text-[12px] text-faint"
          >
            <i-lucide-loader-circle class="h-3.5 w-3.5 animate-spin" />
            加载输出中…
          </div>
          <div v-else-if="failed" class="flex flex-col items-center gap-2 py-8">
            <p class="text-[12px] text-danger-10">输出加载失败</p>
            <button
              type="button"
              class="h-7 control-surface rounded-md px-2.5 text-[12px] control-focus"
              @click="loadTail"
            >
              重试
            </button>
          </div>
          <pre
            v-else
            class="max-h-[60vh] overflow-auto whitespace-pre-wrap break-words rounded-md bg-surface px-2.5 py-1.5 text-[11.5px] text-neutral-10 leading-[1.55] font-mono"
            >{{ output || "（无输出）" }}</pre
          >
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
import { onMounted, ref } from "vue";
import client from "@/api/client";
import { formatBackgroundBytes } from "@/composables/useBackgroundTasks";

const props = defineProps<{
  taskId: string;
  command: string;
}>();

const emit = defineEmits<{ close: [] }>();

/** 已加载内容（文本）；空字符串 = 文件确实没有输出。 */
const output = ref<string | null>(null);
/** 下一页（更早内容）的字节偏移；null = 已到文件头。 */
const nextOffset = ref<number | null>(null);
const totalBytes = ref(0);
/** 已加载部分占整个文件的字节数（start 起点到文件尾）。 */
const loadedBytes = ref(0);

const loading = ref(false);
const loadingEarlier = ref(false);
/** 首次加载失败（内容尚不可展示，可整页重试）。 */
const failed = ref(false);
/** 向上翻页失败（保留已加载内容，按钮变重试）。 */
const earlierFailed = ref(false);

/** 首次/重试：取尾部（缺省读尾部 64KiB）。 */
async function loadTail(): Promise<void> {
  if (loading.value) return;
  loading.value = true;
  failed.value = false;
  try {
    const { data, error } = await client.GET(
      "/api/background-tasks/{id}/output",
      { params: { path: { id: props.taskId } } },
    );
    if (error || !data) {
      failed.value = true;
      return;
    }
    output.value = data.content;
    nextOffset.value = data.next_offset ?? null;
    totalBytes.value = data.total_bytes;
    loadedBytes.value = data.total_bytes - data.start;
  } catch {
    // 网络层异常：与 HTTP 错误同路径，允许重试。
    failed.value = true;
  } finally {
    loading.value = false;
  }
}

/** 向上翻页：offset = next_offset，老内容前插；失败保留已加载内容。 */
async function loadEarlier(): Promise<void> {
  const offset = nextOffset.value;
  if (offset === null || loadingEarlier.value || loading.value) return;
  loadingEarlier.value = true;
  earlierFailed.value = false;
  try {
    const { data, error } = await client.GET(
      "/api/background-tasks/{id}/output",
      { params: { path: { id: props.taskId }, query: { offset } } },
    );
    if (error || !data) {
      earlierFailed.value = true;
      return;
    }
    output.value = data.content + (output.value ?? "");
    nextOffset.value = data.next_offset ?? null;
    totalBytes.value = data.total_bytes;
    loadedBytes.value = data.total_bytes - data.start;
  } catch {
    earlierFailed.value = true;
  } finally {
    loadingEarlier.value = false;
  }
}

onMounted(() => {
  void loadTail();
});

function handleUpdateOpen(open: boolean) {
  // Escape / 遮罩点击关 Dialog 的语义即关闭。
  if (!open) emit("close");
}
</script>
