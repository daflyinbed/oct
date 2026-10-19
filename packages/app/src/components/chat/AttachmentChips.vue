<template>
  <div class="flex flex-col gap-1 px-3 pt-2.5">
    <!-- 拒收提示（超限/前置校验失败）：单行即时文案 -->
    <div v-if="notice" class="text-[11.5px] text-danger-10">{{ notice }}</div>
    <div class="flex flex-wrap items-start gap-1.5">
      <template v-for="item in items" :key="item.localId">
        <!-- 图片：48px 缩略图 + 进度环 -->
        <div v-if="item.objectUrl" class="flex flex-col gap-1">
          <div
            class="group relative h-12 w-12 flex-none overflow-hidden border border-line-soft rounded-lg"
          >
            <ImagePreview :src="item.objectUrl" :filename="item.filename">
              <button
                type="button"
                class="h-full w-full cursor-zoom-in control-focus"
                :aria-label="`预览 ${item.filename}`"
              >
                <img
                  :src="item.objectUrl"
                  :alt="item.filename"
                  class="h-full w-full object-cover"
                />
              </button>
            </ImagePreview>
            <!-- 上传进度：SVG 圆环 -->
            <svg
              v-if="item.status === 'uploading'"
              class="pointer-events-none absolute inset-0 h-full w-full -rotate-90"
              viewBox="0 0 36 36"
            >
              <circle
                cx="18"
                cy="18"
                r="15.9"
                fill="none"
                stroke-width="3"
                class="stroke-neutral-1/70"
                stroke-dasharray="100 100"
              />
              <circle
                cx="18"
                cy="18"
                r="15.9"
                fill="none"
                stroke-width="3"
                stroke-linecap="round"
                class="stroke-accent-8"
                :stroke-dasharray="`${item.progress} 100`"
                pathLength="100"
              />
            </svg>
            <span
              v-else-if="item.status === 'failed'"
              class="pointer-events-none absolute inset-0 flex items-center justify-center bg-danger-10/70 text-[10px] text-white"
              >失败</span
            >
            <button
              type="button"
              class="absolute right-0.5 top-0.5 h-4 w-4 flex items-center justify-center rounded-full bg-neutral-10/70 text-white opacity-0 transition-opacity group-hover:opacity-100"
              title="移除"
              @click="$emit('remove', item.localId)"
            >
              <i-lucide-x class="h-2.5 w-2.5" />
            </button>
          </div>
          <div
            v-if="item.status === 'failed'"
            class="max-w-[220px] flex items-start gap-1 text-[10.5px] text-danger-10"
          >
            <span role="alert" class="min-w-0 break-words">{{
              item.error ?? "上传失败"
            }}</span>
            <button
              type="button"
              class="flex-none underline"
              title="重试"
              @click="$emit('retry', item.localId)"
            >
              重试
            </button>
          </div>
        </div>
        <!-- 非图片：文件卡 -->
        <div
          v-else
          class="group h-12 max-w-[220px] flex flex-none items-center gap-1.5 border border-line-soft rounded-lg bg-surface px-2"
        >
          <i-lucide-file-text class="h-4 w-4 flex-none text-faint" />
          <div class="min-w-0 flex flex-1 flex-col">
            <span class="truncate text-[11.5px] text-neutral-10">{{
              item.filename
            }}</span>
            <span
              v-if="item.status === 'failed'"
              class="flex items-center gap-1 text-[10.5px] text-danger-10"
            >
              <span class="truncate">{{ item.error ?? "上传失败" }}</span>
              <button
                type="button"
                class="flex-none underline"
                title="重试"
                @click="$emit('retry', item.localId)"
              >
                <i-lucide-rotate-ccw class="inline h-3 w-3" />
              </button>
            </span>
            <span
              v-else-if="item.status === 'uploading'"
              class="text-[10.5px] text-faint"
              >{{ item.progress }}%</span
            >
            <span v-else class="text-[10.5px] text-faint">{{
              formatSize(item.sizeBytes)
            }}</span>
          </div>
          <button
            type="button"
            class="h-5 w-5 flex flex-none items-center justify-center rounded text-faint opacity-0 transition-opacity hover:bg-neutral-2 hover:text-neutral-10 group-hover:opacity-100"
            title="移除"
            @click="$emit('remove', item.localId)"
          >
            <i-lucide-x class="h-3 w-3" />
          </button>
        </div>
      </template>
    </div>
  </div>
</template>

<script setup lang="ts">
import ImagePreview from "@/components/ui/ImagePreview.vue";
import type { PendingAttachment } from "@/composables/useAttachments";

defineProps<{
  items: readonly PendingAttachment[];
  /** 前置校验的拒收提示（null = 不展示）。 */
  notice: string | null;
}>();
defineEmits<{
  remove: [localId: string];
  retry: [localId: string];
}>();

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}
</script>
