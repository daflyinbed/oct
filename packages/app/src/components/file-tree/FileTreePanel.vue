<template>
  <aside v-if="visible" class="workspace-panel h-full w-full bg-panel">
    <header
      class="workspace-panel-header select-none justify-between border-b border-line-soft"
    >
      <div
        class="flex items-center gap-[7px] text-[12.5px] text-neutral-10 font-semibold"
      >
        <i-lucide-files class="h-3.5 w-3.5 text-dim" />
        Files
      </div>
      <button
        class="h-[22px] w-[22px] control-ghost rounded-md text-[12px] control-focus"
        title="关闭"
        @click="$emit('close')"
      >
        ✕
      </button>
    </header>

    <div class="workspace-panel-body p-1.5">
      <p v-if="!projectId" class="px-2 py-1.5 text-[12.5px] text-faint">
        Select a conversation to browse its project files.
      </p>
      <p v-else-if="loading" class="px-2 py-1.5 text-[12.5px] text-faint">
        Loading…
      </p>
      <p v-else-if="error" class="px-2 py-1.5 text-[12.5px] text-danger-10">
        {{ error }}
      </p>
      <TreeRoot
        v-else
        v-slot="{ flattenItems }"
        v-model:expanded="expanded"
        class="files-tree outline-none"
        :items="items"
        :get-key="getKey"
        :get-children="getChildren"
        @mouseleave="hoveredPath = null"
      >
        <p
          v-if="flattenItems.length === 0"
          class="px-2 py-1.5 text-[12.5px] text-faint"
        >
          Empty directory.
        </p>
        <TreeItem
          v-for="item in flattenItems"
          :key="item._id"
          v-bind="item.bind"
          v-slot="{ isExpanded, isSelected }"
          class="cursor-pointer rounded-[5px] outline-none"
        >
          <div
            class="h-6 flex items-center gap-1 rounded-[5px] pr-2 text-[12.5px] transition-colors"
            :class="
              isSelected
                ? 'bg-accent-3 text-neutral-10'
                : 'text-dim hover:bg-neutral-1'
            "
            @mouseenter="hoveredPath = item.value.path"
            @click="
              item.value.kind === 'file' && $emit('openFile', item.value.path)
            "
          >
            <!-- Per-level indent guide lanes, styled like pierre's file tree:
                 1px vertical lines that fade in on hover, ancestor column of
                 the hovered row lights up fully. -->
            <span
              v-for="level in item.level - 1"
              :key="level"
              class="relative h-full w-4 shrink-0"
            >
              <span
                class="tree-guide absolute left-1/2 top-0 h-full w-px bg-neutral-5 opacity-0 transition-opacity duration-150 -translate-x-1/2 [.files-tree:hover_&]:opacity-75"
                :class="
                  isGuideActive(item.value.path, level) ? 'opacity-100!' : ''
                "
              />
            </span>
            <i-lucide-chevron-right
              v-if="item.hasChildren"
              class="h-3.5 w-3.5 shrink-0 text-faint transition-transform duration-150"
              :class="isExpanded ? 'rotate-90' : ''"
            />
            <span v-else class="w-3.5 shrink-0" />
            <component
              :is="resolveFileIcon(item.value)"
              v-if="item.value.kind === 'file'"
              class="h-3.5 w-3.5 shrink-0"
            />
            <span class="truncate">{{ item.value.name }}</span>
          </div>
        </TreeItem>
      </TreeRoot>
    </div>
  </aside>
</template>

<script setup lang="ts">
import { TreeItem, TreeRoot } from "reka-ui";
import { ref, watch } from "vue";
import { useFileTree } from "@/composables/useFileTree";
import { resolveFileIcon } from "@/utils/fileIcons";
import type { TreeNode } from "@/composables/useFileTree";

const props = defineProps<{
  visible: boolean;
  projectId: string | null;
}>();

defineEmits<{
  close: [];
  openFile: [path: string];
}>();

const { items, expanded, loading, error, loadRoot } = useFileTree();

const hoveredPath = ref<string | null>(null);

// A guide at `level` of `rowPath` is active when the hovered row sits inside
// that guide's ancestor directory — the whole ancestor column lights up.
function isGuideActive(rowPath: string, level: number): boolean {
  if (hoveredPath.value === null) return false;
  const ancestor = rowPath.split("/").slice(0, level).join("/");
  return (
    hoveredPath.value === ancestor ||
    hoveredPath.value.startsWith(`${ancestor}/`)
  );
}

function getKey(item: TreeNode): string {
  return item.path;
}

function getChildren(item: TreeNode): TreeNode[] | undefined {
  return item.kind === "dir" ? (item.children ?? []) : undefined;
}

watch(
  () => props.projectId,
  (id) => {
    if (id) loadRoot(id);
  },
  { immediate: true },
);
</script>
