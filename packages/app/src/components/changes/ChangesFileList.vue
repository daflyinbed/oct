<template>
  <div>
    <template v-for="row in rows" :key="row.key">
      <div
        v-if="row.files"
        class="group h-6 flex items-center gap-1 rounded-[5px] pr-2 text-base text-dim transition-colors hover:bg-neutral-1"
        :style="{ paddingLeft: `${8 + row.depth * 14}px` }"
      >
        <button
          class="min-w-0 flex flex-1 items-center gap-1 text-left"
          @click="toggle(row.key)"
        >
          <i-lucide-chevron-right
            class="h-3.5 w-3.5 shrink-0 text-faint transition-transform duration-150"
            :class="collapsed.has(row.key) ? '' : 'rotate-90'"
          />
          <i-lucide-folder v-if="row.folder" class="h-3.5 w-3.5 shrink-0" />
          <span class="truncate">{{ row.label }}</span>
        </button>
        <input
          type="checkbox"
          class="h-3.5 w-3.5 shrink-0 accent-accent-8"
          :class="
            rowChecked(row) || rowMixed(row)
              ? ''
              : 'opacity-0 group-hover:opacity-100 focus-visible:opacity-100'
          "
          :aria-label="`Stage ${row.label}`"
          :checked="rowChecked(row)"
          :indeterminate="rowMixed(row)"
          :disabled="disabled"
          @change="
            emit(
              'stage',
              row.files.map((f) => f.path),
              !rowChecked(row),
            )
          "
        />
      </div>
      <div
        v-else-if="row.file"
        class="group h-6 flex items-center gap-1 rounded-[5px] pr-2 text-base transition-colors hover:bg-neutral-1"
        :class="selected === row.file.path ? 'bg-accent-3 text-neutral-10' : ''"
        :style="{ paddingLeft: `${20 + row.depth * 14}px` }"
      >
        <button
          class="min-w-0 flex flex-1 items-center gap-2 text-left"
          :title="
            row.file.original_path
              ? `${row.file.original_path} → ${row.file.path}`
              : row.file.path
          "
          @click="emit('open', { path: row.file.path, staged: row.staged })"
        >
          <span
            class="w-3 shrink-0 text-center font-mono"
            :class="
              row.file.conflicted
                ? 'text-danger-10'
                : row.file.untracked
                  ? 'text-success-10'
                  : 'text-warning-10'
            "
            >{{
              row.file.conflicted
                ? "!"
                : row.file.untracked
                  ? "+"
                  : row.file.index.trim() || row.file.worktree.trim()
            }}</span
          >
          <span class="min-w-0 flex-1 truncate">{{ row.label }}</span>
          <span v-if="row.file.binary" class="text-badge text-faint"
            >binary</span
          >
          <span v-else class="flex shrink-0 gap-1 text-xs"
            ><span class="text-success-10"
              >+{{
                row.staged === true
                  ? row.file.staged_additions
                  : row.staged === false
                    ? row.file.unstaged_additions
                    : row.file.additions
              }}</span
            ><span class="text-danger-10"
              >−{{
                row.staged === true
                  ? row.file.staged_deletions
                  : row.staged === false
                    ? row.file.unstaged_deletions
                    : row.file.deletions
              }}</span
            ></span
          >
        </button>
        <input
          type="checkbox"
          class="h-3.5 w-3.5 shrink-0 accent-accent-8"
          :class="
            rowChecked(row) || rowMixed(row)
              ? ''
              : 'opacity-0 group-hover:opacity-100 focus-visible:opacity-100'
          "
          :aria-label="`Stage ${row.file.path}`"
          :checked="rowChecked(row)"
          :indeterminate="rowMixed(row)"
          :disabled="disabled"
          @change="emit('stage', [row.file.path], !rowChecked(row))"
        />
      </div>
    </template>
  </div>
</template>
<script setup lang="ts">
import { computed, ref } from "vue";
import type { DiffSelection, GitFile } from "@/composables/useGit";

const props = defineProps<{
  files: GitFile[];
  view: string;
  grouping: string;
  disabled: boolean;
  selected?: string;
}>();
const emit = defineEmits<{
  stage: [paths: string[], stage: boolean];
  open: [selection: DiffSelection];
}>();
const collapsed = ref(new Set<string>());
function toggle(key: string) {
  const next = new Set(collapsed.value);
  if (next.has(key)) next.delete(key);
  else next.add(key);
  collapsed.value = next;
}
function hasStaged(f: GitFile) {
  return !f.untracked && f.index !== " ";
}
function fullyStaged(f: GitFile) {
  return hasStaged(f) && f.worktree === " ";
}
interface Row {
  key: string;
  label: string;
  depth: number;
  files?: GitFile[];
  file?: GitFile;
  staged?: boolean;
  folder?: boolean;
}
function rowChecked(row: Row) {
  if (row.staged !== undefined) return row.staged;
  return (row.files ?? [row.file!]).every(fullyStaged);
}
function rowMixed(row: Row) {
  return (
    row.staged === undefined &&
    !rowChecked(row) &&
    (row.files ?? [row.file!]).some(hasStaged)
  );
}
const rows = computed(() => {
  const result: Row[] = [];
  const groups =
    props.grouping === "tracked"
      ? [
          {
            label: "Tracked",
            files: props.files.filter((f) => !f.untracked),
            staged: undefined,
          },
          {
            label: "Untracked",
            files: props.files.filter((f) => f.untracked),
            staged: undefined,
          },
        ]
      : props.grouping === "staged"
        ? [
            {
              label: "Staged",
              files: props.files.filter(hasStaged),
              staged: true,
            },
            {
              label: "Unstaged",
              files: props.files.filter(
                (f) => f.untracked || f.worktree !== " ",
              ),
              staged: false,
            },
          ]
        : [{ label: "", files: props.files, staged: undefined }];
  function tree(
    files: GitFile[],
    prefix: string,
    depth: number,
    key: string,
    staged?: boolean,
  ) {
    const dirs = new Map<string, GitFile[]>();
    const leaves: GitFile[] = [];
    for (const f of files) {
      const remainder = f.path.slice(prefix.length);
      const slash = remainder.indexOf("/");
      if (props.view === "tree" && slash >= 0) {
        const dir = remainder.slice(0, slash);
        dirs.set(dir, [...(dirs.get(dir) ?? []), f]);
      } else leaves.push(f);
    }
    for (const [dir, children] of [...dirs].toSorted(([a], [b]) =>
      a.localeCompare(b),
    )) {
      const id = `${key}/${dir}`;
      result.push({
        key: id,
        label: dir,
        depth,
        files: children,
        staged,
        folder: true,
      });
      if (!collapsed.value.has(id))
        tree(children, `${prefix}${dir}/`, depth + 1, id, staged);
    }
    for (const f of leaves.toSorted((a, b) => a.path.localeCompare(b.path)))
      result.push({
        key: `${key}/${f.path}`,
        label: props.view === "tree" ? f.path.slice(prefix.length) : f.path,
        depth,
        file: f,
        staged,
      });
  }
  for (const group of groups) {
    if (group.files.length === 0) continue;
    if (group.label)
      result.push({
        key: group.label,
        label: group.label,
        depth: 0,
        files: group.files,
        staged: group.staged,
      });
    if (!collapsed.value.has(group.label))
      tree(group.files, "", group.label ? 1 : 0, group.label, group.staged);
  }
  return result;
});
</script>
