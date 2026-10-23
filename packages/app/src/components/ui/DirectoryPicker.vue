<template>
  <ComboboxRoot
    ref="combobox"
    v-model:open="open"
    :reset-search-term-on-blur="false"
    :reset-search-term-on-select="false"
    ignore-filter
    open-on-focus
    open-on-click
    @highlight="highlighted = $event?.value"
  >
    <ComboboxAnchor class="relative w-full">
      <ComboboxInput
        ref="input"
        v-model="model"
        :aria-label="label"
        :placeholder="placeholder"
        spellcheck="false"
        class="h-[30px] w-full control-surface pl-3 pr-8 text-base font-mono control-focus"
        @keydown.capture="onKeydown"
      />
      <ComboboxTrigger
        type="button"
        :aria-label="`浏览${label}`"
        class="absolute right-1 top-1/2 h-6 w-6 flex items-center justify-center control-ghost -translate-y-1/2"
      >
        <i-lucide-folder-open class="h-3.5 w-3.5" />
      </ComboboxTrigger>
    </ComboboxAnchor>

    <ComboboxPortal>
      <ComboboxContent
        position="popper"
        align="start"
        :side-offset="4"
        class="z-50 w-[var(--reka-combobox-trigger-width)] overflow-hidden border border-line-soft rounded-lg bg-panel shadow-[var(--oct-shadow)]"
        @escape-key-down.stop
      >
        <div
          v-if="listing"
          class="flex items-center gap-1 border-b border-line-soft px-2 py-1"
        >
          <button
            type="button"
            title="返回上级目录（Alt+↑）"
            aria-label="返回上级目录"
            class="h-6 w-6 flex shrink-0 items-center justify-center control-ghost disabled:opacity-40"
            :disabled="!listing.parent"
            @click="goToParent"
          >
            <i-lucide-arrow-up class="h-3.5 w-3.5" />
          </button>
          <span
            :title="listing.directory"
            class="min-w-0 truncate text-xs text-faint font-mono"
          >
            {{ listing.directory }}
          </span>
        </div>
        <ComboboxViewport
          class="max-h-[min(260px,var(--reka-combobox-content-available-height))] overflow-y-auto p-1"
        >
          <p v-if="loading" role="status" class="px-2 py-2 text-sm text-faint">
            正在读取目录…
          </p>
          <p
            v-else-if="error"
            role="alert"
            class="px-2 py-2 text-sm text-danger-10"
          >
            {{ error }}
          </p>
          <template v-else-if="listing">
            <ComboboxItem
              v-if="!listing.query"
              value="use-directory"
              class="directory-option"
              @select.prevent="confirm"
            >
              <i-lucide-corner-down-right
                class="h-3.5 w-3.5 shrink-0 text-faint"
              />
              <span>使用此目录</span>
              <span class="ml-auto text-xs text-faint">↵</span>
            </ComboboxItem>
            <ComboboxItem
              v-for="entry in listing.entries"
              :key="entry.path"
              :value="entry.path"
              :text-value="entry.name"
              class="directory-option"
              @select.prevent="enter(entry.path)"
            >
              <i-lucide-folder class="h-3.5 w-3.5 shrink-0 text-faint" />
              <span class="truncate">
                <span
                  v-for="(part, index) in highlightedName(
                    entry.name,
                    listing.query,
                  )"
                  :key="index"
                  :class="part.match ? 'text-accent-10 font-semibold' : ''"
                  >{{ part.text }}</span
                >
              </span>
            </ComboboxItem>
            <p
              v-if="listing.entries.length === 0"
              class="px-2 py-2 text-sm text-faint"
            >
              {{ listing.query ? "没有匹配的目录" : "此目录下没有子目录" }}
            </p>
          </template>
        </ComboboxViewport>
        <div class="border-t border-line-soft px-3 py-1.5 text-xs text-faint">
          ↑↓ 选择 · Tab 进入 · ↵ 确认 · Esc 收起
        </div>
      </ComboboxContent>
    </ComboboxPortal>
  </ComboboxRoot>
</template>

<script setup lang="ts">
import {
  ComboboxAnchor,
  ComboboxContent,
  ComboboxInput,
  ComboboxItem,
  ComboboxPortal,
  ComboboxRoot,
  ComboboxTrigger,
  ComboboxViewport,
} from "reka-ui";
import { nextTick, ref, useTemplateRef, watch } from "vue";
import client from "@/api/client";
import type { components } from "@/api/schema";

defineProps<{ label: string; placeholder?: string }>();
const model = defineModel<string>({ required: true });
const emit = defineEmits<{ select: [path: string] }>();
const open = ref(false);
const loading = ref(false);
const error = ref<string | null>(null);
const listing = ref<components["schemas"]["DirectoryListing"] | null>(null);
const highlighted = ref<unknown>();
const combobox = useTemplateRef("combobox");
const input = useTemplateRef("input");

const withSlash = (path: string) => (path.endsWith("/") ? path : `${path}/`);

watch([model, open], ([path, isOpen], _, onCleanup) => {
  listing.value = null;
  highlighted.value = undefined;
  error.value = null;
  loading.value = isOpen;
  if (!isOpen) return;
  const controller = new AbortController();
  const timer = setTimeout(async () => {
    try {
      const { data, error: apiError } = await client.GET("/api/directories", {
        params: { query: { path } },
        signal: controller.signal,
      });
      if (controller.signal.aborted) return;
      if (apiError || !data) {
        error.value = "无法读取目录，请检查路径和访问权限。";
      } else if (!path) {
        model.value = withSlash(data.directory);
      } else {
        listing.value = data;
        loading.value = false;
        await nextTick();
        combobox.value?.highlightFirstItem?.();
      }
    } catch {
      if (!controller.signal.aborted)
        error.value = "读取目录失败，请检查连接后重试。";
    } finally {
      if (!controller.signal.aborted) loading.value = false;
    }
  }, 120);
  onCleanup(() => {
    clearTimeout(timer);
    controller.abort();
  });
});

async function enter(path: string) {
  model.value = withSlash(path);
  // The clicked option is removed when the next directory loads. Keep typing
  // and keyboard navigation in the path field instead of losing focus to body.
  await nextTick();
  (input.value?.$el as HTMLInputElement | undefined)?.focus();
}

function confirm() {
  if (!listing.value || loading.value) return;
  model.value = listing.value.directory;
  (input.value?.$el as HTMLInputElement | undefined)?.focus();
  open.value = false;
  emit("select", model.value);
}

function goToParent() {
  if (listing.value?.parent) enter(listing.value.parent);
}

function onKeydown(event: KeyboardEvent) {
  if (event.isComposing) return;
  if (event.altKey && event.key === "ArrowUp" && open.value) {
    event.preventDefault();
    event.stopPropagation();
    goToParent();
  } else if (event.key === "Enter") {
    // Selecting a directory must not also submit the surrounding project form.
    event.preventDefault();
    event.stopPropagation();
    if (!open.value) {
      open.value = true;
    } else if (!loading.value && listing.value) {
      if (highlighted.value === "use-directory") confirm();
      else if (typeof highlighted.value === "string") enter(highlighted.value);
    }
  } else if (
    event.key === "Tab" &&
    !event.shiftKey &&
    open.value &&
    !loading.value
  ) {
    const path =
      highlighted.value === "use-directory"
        ? listing.value?.entries[0]?.path
        : highlighted.value;
    if (typeof path === "string") {
      event.preventDefault();
      event.stopPropagation();
      enter(path);
    }
  }
}

function highlightedName(name: string, query: string) {
  const needle = [...query.toLocaleLowerCase()];
  let index = 0;
  return [...name].map((text) => {
    const match =
      index < needle.length && text.toLocaleLowerCase() === needle[index];
    if (match) index++;
    return { text, match };
  });
}
</script>

<style scoped>
.directory-option {
  display: flex;
  align-items: center;
  gap: 0.5rem;
  min-height: 28px;
  padding: 3px 8px;
  border-radius: 4px;
  cursor: pointer;
  outline: none;
  color: var(--oct-color-strong);
  font-size: var(--oct-font-size-sm);
  line-height: var(--oct-line-height-sm);
}

.directory-option[data-highlighted] {
  background: var(--oct-color-neutral-3);
}
</style>
