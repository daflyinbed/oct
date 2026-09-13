<template>
  <aside v-if="visible" class="workspace-panel h-full w-full bg-panel">
    <header
      class="workspace-panel-header justify-between border-b border-neutral-5"
    >
      <span class="text-[1rem] font-medium text-neutral-10">Files</span>
      <button
        class="control-ghost control-focus p-1 text-[0.875rem] rounded-sm"
        @click="$emit('close')"
      >
        ✕
      </button>
    </header>

    <div class="workspace-panel-body p-2">
      <p
        v-if="!projectId"
        class="px-3 py-2 text-[0.9rem] text-neutral-10/60"
      >
        Select a conversation to browse its project files.
      </p>
      <p v-else-if="loading" class="px-3 py-2 text-[0.9rem] text-neutral-10/60">
        Loading…
      </p>
      <p v-else-if="error" class="px-3 py-2 text-[0.9rem] text-danger-10">
        {{ error }}
      </p>
      <TreeRoot
        v-else
        v-slot="{ flattenItems }"
        class="files-tree outline-none"
        v-model:expanded="expanded"
        :items="items"
        :get-key="getKey"
        :get-children="getChildren"
        @mouseleave="hoveredPath = null"
      >
        <p
          v-if="flattenItems.length === 0"
          class="px-3 py-2 text-[0.9rem] text-neutral-10/60"
        >
          Empty directory.
        </p>
        <TreeItem
          v-for="item in flattenItems"
          :key="item._id"
          v-bind="item.bind"
          v-slot="{ isExpanded, isSelected }"
          class="cursor-pointer rounded-sm outline-none"
        >
          <div
            class="flex h-8 items-center gap-1 rounded-sm pr-2 text-[0.9rem] transition-colors"
            :class="
              isSelected
                ? 'bg-accent-2 text-neutral-10'
                : 'text-neutral-10 hover:bg-neutral-1'
            "
            @mouseenter="hoveredPath = item.value.path"
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
                class="tree-guide absolute top-0 left-1/2 h-full w-px -translate-x-1/2 bg-neutral-5 opacity-0 transition-opacity duration-150 [.files-tree:hover_&]:opacity-75"
                :class="
                  isGuideActive(item.value.path, level) ? 'opacity-100!' : ''
                "
              />
            </span>
            <i-lucide-chevron-right
              v-if="item.hasChildren"
              class="h-4 w-4 shrink-0 text-neutral-10/60 transition-transform duration-150"
              :class="isExpanded ? 'rotate-90' : ''"
            />
            <span v-else class="w-4 shrink-0" />
            <component
              :is="resolveFileIcon(item.value)"
              v-if="item.value.kind === 'file'"
              class="h-4 w-4 shrink-0"
            />
            <span class="truncate">{{ item.value.name }}</span>
          </div>
        </TreeItem>
      </TreeRoot>
    </div>
  </aside>
</template>

<script setup lang="ts">
import { ref, watch, type Component } from "vue";
import { TreeItem, TreeRoot } from "reka-ui";
import { useFileTree, type TreeNode } from "@/composables/useFileTree";

import IconTypeScript from "~icons/vscode-icons/file-type-typescript";
import IconJs from "~icons/vscode-icons/file-type-js";
import IconJson from "~icons/vscode-icons/file-type-json";
import IconVue from "~icons/vscode-icons/file-type-vue";
import IconCss from "~icons/vscode-icons/file-type-css";
import IconSass from "~icons/vscode-icons/file-type-sass";
import IconHtml from "~icons/vscode-icons/file-type-html";
import IconMarkdown from "~icons/vscode-icons/file-type-markdown";
import IconRust from "~icons/vscode-icons/file-type-rust";
import IconPython from "~icons/vscode-icons/file-type-python";
import IconGo from "~icons/vscode-icons/file-type-go";
import IconRuby from "~icons/vscode-icons/file-type-ruby";
import IconShell from "~icons/vscode-icons/file-type-shell";
import IconYaml from "~icons/vscode-icons/file-type-yaml";
import IconToml from "~icons/vscode-icons/file-type-toml";
import IconSql from "~icons/vscode-icons/file-type-sql";
import IconSqlite from "~icons/vscode-icons/file-type-sqlite";
import IconSvg from "~icons/vscode-icons/file-type-svg";
import IconImage from "~icons/vscode-icons/file-type-image";
import IconZip from "~icons/vscode-icons/file-type-zip";
import IconText from "~icons/vscode-icons/file-type-text";
import IconC from "~icons/vscode-icons/file-type-c";
import IconCpp from "~icons/vscode-icons/file-type-cpp";
import IconSwift from "~icons/vscode-icons/file-type-swift";
import IconGraphql from "~icons/vscode-icons/file-type-graphql";
import IconWasm from "~icons/vscode-icons/file-type-wasm";
import IconDocker from "~icons/vscode-icons/file-type-docker";
import IconGit from "~icons/vscode-icons/file-type-git";
import IconNpm from "~icons/vscode-icons/file-type-npm";
import IconVite from "~icons/vscode-icons/file-type-vite";
import IconFile from "~icons/lucide/file";

const props = defineProps<{
  visible: boolean;
  projectId: string | null;
}>();

defineEmits<{
  close: [];
}>();

const { items, expanded, loading, error, loadRoot } = useFileTree();

// Icon resolution follows pierre's rule order: exact filename first, then
// extension, then the generic file glyph.
const FILENAME_ICONS: Record<string, Component> = {
  "package.json": IconNpm,
  "package-lock.json": IconNpm,
  "pnpm-lock.yaml": IconNpm,
  dockerfile: IconDocker,
  "docker-compose.yml": IconDocker,
  "docker-compose.yaml": IconDocker,
  "compose.yml": IconDocker,
  "compose.yaml": IconDocker,
  ".gitignore": IconGit,
  ".gitattributes": IconGit,
  ".gitmodules": IconGit,
  ".gitkeep": IconGit,
};

const EXTENSION_ICONS: Record<string, Component> = {
  ts: IconTypeScript,
  mts: IconTypeScript,
  cts: IconTypeScript,
  tsx: IconTypeScript,
  js: IconJs,
  mjs: IconJs,
  cjs: IconJs,
  jsx: IconJs,
  json: IconJson,
  jsonc: IconJson,
  json5: IconJson,
  vue: IconVue,
  css: IconCss,
  postcss: IconCss,
  scss: IconSass,
  sass: IconSass,
  less: IconSass,
  styl: IconSass,
  html: IconHtml,
  htm: IconHtml,
  md: IconMarkdown,
  mdx: IconMarkdown,
  markdown: IconMarkdown,
  rs: IconRust,
  py: IconPython,
  pyi: IconPython,
  go: IconGo,
  rb: IconRuby,
  erb: IconRuby,
  sh: IconShell,
  bash: IconShell,
  zsh: IconShell,
  fish: IconShell,
  yaml: IconYaml,
  yml: IconYaml,
  toml: IconToml,
  sql: IconSql,
  db: IconSqlite,
  sqlite: IconSqlite,
  sqlite3: IconSqlite,
  svg: IconSvg,
  png: IconImage,
  jpg: IconImage,
  jpeg: IconImage,
  gif: IconImage,
  webp: IconImage,
  bmp: IconImage,
  ico: IconImage,
  avif: IconImage,
  zip: IconZip,
  tar: IconZip,
  gz: IconZip,
  tgz: IconZip,
  "7z": IconZip,
  txt: IconText,
  log: IconText,
  ini: IconText,
  cfg: IconText,
  c: IconC,
  h: IconC,
  cc: IconCpp,
  cpp: IconCpp,
  cxx: IconCpp,
  hpp: IconCpp,
  swift: IconSwift,
  gql: IconGraphql,
  graphql: IconGraphql,
  wasm: IconWasm,
};

function resolveFileIcon(node: TreeNode): Component {
  if (node.kind === "dir") return IconFile;

  const lowerName = node.name.toLowerCase();
  const exact = FILENAME_ICONS[lowerName];
  if (exact) return exact;
  if (lowerName.startsWith("vite.config")) return IconVite;
  if (lowerName.startsWith(".env")) return IconText;

  const extension = lowerName.split(".").pop();
  return (extension && EXTENSION_ICONS[extension]) || IconFile;
}

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
