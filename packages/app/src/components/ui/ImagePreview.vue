<template>
  <DialogRoot v-model:open="open">
    <DialogTrigger as-child>
      <slot />
    </DialogTrigger>
    <DialogPortal>
      <DialogOverlay class="fixed inset-0 z-50 bg-black/80" />
      <DialogContent
        class="fixed inset-0 z-50 overflow-hidden text-white outline-none"
        @click.self="open = false"
      >
        <div
          class="absolute inset-x-0 top-0 z-10 flex items-center gap-4 bg-gradient-to-b from-black/40 px-5 py-4"
        >
          <DialogTitle class="min-w-0 flex-1 truncate text-sm font-medium">
            {{ filename }}
          </DialogTitle>
          <DialogClose
            :class="TOOL_BUTTON_CLASS"
            title="关闭预览（Esc）"
            aria-label="关闭预览"
          >
            <i-lucide-x class="h-5 w-5" aria-hidden="true" />
          </DialogClose>
        </div>
        <DialogDescription class="sr-only">
          拖动图片可移动，滚轮可缩放，按 Esc 或点击背景关闭预览。
        </DialogDescription>

        <div
          ref="stage"
          class="absolute inset-x-4 bottom-24 top-20 flex items-center justify-center sm:inset-x-12"
          @click.self="open = false"
          @wheel.prevent="handleWheel"
        >
          <i-lucide-loader-circle
            v-if="loading"
            class="absolute h-7 w-7 animate-spin text-white/70"
            aria-label="加载图片中"
          />
          <p v-if="failed" role="alert" class="text-sm text-white/80">
            图片加载失败
          </p>
          <img
            v-show="!failed"
            :src="src"
            :alt="filename"
            :draggable="false"
            class="touch-none select-none object-contain"
            :class="[
              loading ? 'opacity-0' : '',
              drag
                ? 'cursor-grabbing'
                : 'cursor-grab transition-transform duration-150',
            ]"
            :style="imageStyle"
            @load="loading = false"
            @error="
              loading = false;
              failed = true;
            "
            @click.stop
            @pointerdown="startDrag"
            @pointermove="moveDrag"
            @pointerup="drag = null"
            @pointercancel="drag = null"
            @lostpointercapture="drag = null"
          />
        </div>

        <div
          class="absolute bottom-6 left-1/2 z-10 max-w-[calc(100vw-24px)] flex items-center gap-1 rounded-full bg-black/70 p-2 shadow-lg -translate-x-1/2"
        >
          <button
            type="button"
            :class="TOOL_BUTTON_CLASS"
            :disabled="loading || failed"
            title="向左旋转"
            aria-label="向左旋转"
            @click="rotation -= 90"
          >
            <i-lucide-rotate-ccw class="h-5 w-5" aria-hidden="true" />
          </button>
          <button
            type="button"
            :class="TOOL_BUTTON_CLASS"
            :disabled="loading || failed"
            title="向右旋转"
            aria-label="向右旋转"
            @click="rotation += 90"
          >
            <i-lucide-rotate-cw class="h-5 w-5" aria-hidden="true" />
          </button>
          <button
            type="button"
            :class="TOOL_BUTTON_CLASS"
            :disabled="loading || failed"
            title="重置视图"
            aria-label="重置视图"
            @click="resetView"
          >
            <i-lucide-scan class="h-5 w-5" aria-hidden="true" />
          </button>
          <span class="mx-1 h-5 w-px flex-none bg-white/20" />
          <button
            type="button"
            :class="TOOL_BUTTON_CLASS"
            :disabled="loading || failed || scale <= MIN_SCALE"
            title="缩小"
            aria-label="缩小"
            @click="zoom(1 / ZOOM_FACTOR)"
          >
            <i-lucide-zoom-out class="h-5 w-5" aria-hidden="true" />
          </button>
          <span
            class="w-10 flex-none text-center text-xs text-white/70 tabular-nums"
          >
            {{ Math.round(scale * 100) }}%
          </span>
          <button
            type="button"
            :class="TOOL_BUTTON_CLASS"
            :disabled="loading || failed || scale >= MAX_SCALE"
            title="放大"
            aria-label="放大"
            @click="zoom(ZOOM_FACTOR)"
          >
            <i-lucide-zoom-in class="h-5 w-5" aria-hidden="true" />
          </button>
          <a
            :href="src"
            :download="filename"
            :class="TOOL_BUTTON_CLASS"
            title="下载图片"
            aria-label="下载图片"
          >
            <i-lucide-download class="h-5 w-5" aria-hidden="true" />
          </a>
        </div>
      </DialogContent>
    </DialogPortal>
  </DialogRoot>
</template>

<script setup lang="ts">
import { useElementSize } from "@vueuse/core";
import {
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogOverlay,
  DialogPortal,
  DialogRoot,
  DialogTitle,
  DialogTrigger,
} from "reka-ui";
import { computed, reactive, ref, watch } from "vue";

const props = defineProps<{
  src: string;
  filename: string;
}>();

const TOOL_BUTTON_CLASS =
  "flex h-9 w-9 flex-none items-center justify-center rounded-full text-white/80 outline-none transition-colors hover:bg-white/15 hover:text-white focus-visible:ring-2 focus-visible:ring-white disabled:cursor-default disabled:opacity-30";
const MIN_SCALE = 0.25;
const MAX_SCALE = 5;
const ZOOM_FACTOR = 1.25;

const open = ref(false);
const stage = ref<HTMLElement>();
const { width, height } = useElementSize(stage);
const loading = ref(true);
const failed = ref(false);
const scale = ref(1);
const rotation = ref(0);
const offset = reactive({ x: 0, y: 0 });
const drag = ref<{
  x: number;
  y: number;
  offsetX: number;
  offsetY: number;
} | null>(null);

const imageStyle = computed(() => {
  // 旋转 90° 后交换约束，初始视图始终能容纳整张图片。
  const rotated = Math.abs(rotation.value % 180) === 90;
  return {
    maxWidth: `${rotated ? height.value : width.value}px`,
    maxHeight: `${rotated ? width.value : height.value}px`,
    transform: `translate(${offset.x}px, ${offset.y}px) rotate(${rotation.value}deg) scale(${scale.value})`,
  };
});

function resetView() {
  scale.value = 1;
  rotation.value = 0;
  offset.x = 0;
  offset.y = 0;
  drag.value = null;
}

watch([open, () => props.src], () => {
  resetView();
  loading.value = true;
  failed.value = false;
});

function zoom(factor: number) {
  if (loading.value || failed.value) return;
  const next = Math.min(MAX_SCALE, Math.max(MIN_SCALE, scale.value * factor));
  const ratio = next / scale.value;
  offset.x *= ratio;
  offset.y *= ratio;
  scale.value = next;
}

function handleWheel(event: WheelEvent) {
  if (event.deltaY !== 0)
    zoom(event.deltaY < 0 ? ZOOM_FACTOR : 1 / ZOOM_FACTOR);
}

function startDrag(event: PointerEvent) {
  if (event.button !== 0 || loading.value || failed.value) return;
  event.preventDefault();
  (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
  drag.value = {
    x: event.clientX,
    y: event.clientY,
    offsetX: offset.x,
    offsetY: offset.y,
  };
}

function moveDrag(event: PointerEvent) {
  if (!drag.value) return;
  offset.x = drag.value.offsetX + event.clientX - drag.value.x;
  offset.y = drag.value.offsetY + event.clientY - drag.value.y;
}
</script>
