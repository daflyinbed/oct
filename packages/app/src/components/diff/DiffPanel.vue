<template>
  <aside v-if="visible" class="workspace-panel h-full w-full bg-panel">
    <header
      class="workspace-panel-header select-none justify-between border-b border-line-soft"
    >
      <div
        class="min-w-0 flex items-center gap-[7px] text-base text-neutral-10 font-semibold"
      >
        <i-lucide-git-compare
          v-if="mode === 'changes'"
          class="h-3.5 w-3.5 shrink-0 text-dim"
        />
        <i-lucide-history v-else class="h-3.5 w-3.5 shrink-0 text-dim" />
        <span class="truncate">{{
          mode === "changes"
            ? "Changes"
            : mode === "stashes"
              ? "Stashes"
              : "History"
        }}</span>
        <span v-if="mode === 'changes' && status" class="text-faint font-normal"
          >({{ status.files.length }})</span
        >
      </div>
      <div class="flex shrink-0 items-center gap-1">
        <div
          class="flex items-center border border-line-soft rounded-md p-0.5 text-xs"
        >
          <button
            class="rounded px-1.5 py-0.5 transition-colors"
            :class="
              mode === 'changes'
                ? 'bg-neutral-3 text-neutral-10'
                : 'text-dim hover:text-neutral-10'
            "
            @click="mode = 'changes'"
          >
            Changes
          </button>
          <button
            class="rounded px-1.5 py-0.5 transition-colors"
            :class="
              mode === 'changes'
                ? 'text-dim hover:text-neutral-10'
                : 'bg-neutral-3 text-neutral-10'
            "
            @click="mode = 'history'"
          >
            History
          </button>
        </div>
        <button
          class="h-[22px] w-[22px] control-ghost rounded-md text-sm control-focus"
          title="关闭 Changes"
          aria-label="关闭 Changes"
          @click="emit('close')"
        >
          ✕
        </button>
      </div>
    </header>
    <p v-if="!projectId" class="px-2 py-1.5 text-base text-faint">
      选择项目会话以查看 Git 变更。
    </p>
    <template v-else>
      <div class="flex shrink-0 items-center gap-1 p-2 text-sm">
        <button
          class="min-w-0 flex flex-1 items-center gap-1.5 control-ghost rounded py-1 text-left"
          :disabled="!status || busy"
          @click="openDiff({})"
        >
          <i-lucide-git-compare class="h-3.5 w-3.5 shrink-0" />
          <span class="truncate">View Diff</span>
          <span class="text-success-10">+{{ totals.additions }}</span
          ><span class="text-danger-10">−{{ totals.deletions }}</span>
        </button>
        <button
          class="control-ghost rounded p-1"
          title="刷新 Git"
          aria-label="刷新 Git"
          :disabled="loading || busy"
          @click="refresh"
        >
          <i-lucide-refresh-cw
            class="h-3 w-3"
            :class="loading ? 'animate-spin' : ''"
          />
        </button>
        <GitMenu label="View options" :groups="viewMenus" @select="setView"
          ><i-lucide-sliders-horizontal class="h-3.5 w-3.5"
        /></GitMenu>
        <div class="flex shrink-0 items-center border border-line-soft rounded">
          <button
            class="control-ghost px-1.5 py-1 disabled:opacity-40"
            :disabled="busy || !hasUnstaged"
            @click="run('stage')"
          >
            Stage All
          </button>
          <GitMenu
            label="Changes actions"
            :disabled="busy || !status"
            :groups="actionMenus"
            @select="selectAction"
          />
        </div>
      </div>
      <p
        v-if="error"
        role="alert"
        class="mx-2 mb-2 whitespace-pre-wrap break-words rounded bg-danger-1 p-2 text-danger-10"
      >
        {{ error }}
      </p>
      <p v-if="busy" role="status" class="px-3 pb-2 text-accent-10">
        Git 操作进行中…
      </p>
      <div class="workspace-panel-body p-1.5">
        <template v-if="mode === 'changes'">
          <ChangesFileList
            v-if="status?.files.length"
            :key="projectId"
            :files="status.files"
            :view="view"
            :grouping="grouping"
            :disabled="busy"
            :selected="selected"
            @open="openDiff"
            @stage="(paths, stage) => run(stage ? 'stage' : 'unstage', paths)"
          />
          <div
            v-else-if="status && !loading"
            class="flex flex-col items-center gap-2 px-4 py-12 text-faint"
          >
            <i-lucide-check class="h-6 w-6 text-success-10" />
            <p>Working tree clean</p>
          </div>
          <p
            v-else-if="loading && !status"
            class="px-2 py-1.5 text-base text-faint"
          >
            加载 Git 状态…
          </p>
        </template>
        <template v-else>
          <div
            v-if="mode === 'stashes'"
            class="mb-1 flex items-center justify-between px-2 py-1 text-base text-dim"
          >
            <span>Stashes</span
            ><button
              class="control-ghost rounded px-2"
              @click="mode = 'changes'"
            >
              返回 Changes
            </button>
          </div>
          <div class="flex flex-col gap-0.5">
            <button
              v-for="entry in mode === 'stashes' ? stashes : history"
              :key="entry.id"
              class="w-full rounded-[5px] px-2 py-1.5 text-left transition-colors hover:bg-neutral-1"
              @click="
                openDiff({ revision: entry.id, stash: mode === 'stashes' })
              "
            >
              <p class="text-neutral-11 truncate text-base">
                {{ entry.subject }}
              </p>
              <p class="mt-0.5 flex gap-2 text-badge text-faint">
                <span class="text-accent-10 font-mono">{{
                  entry.short_id
                }}</span
                ><span class="min-w-0 flex-1 truncate">{{ entry.author }}</span
                ><span>{{ entry.date.slice(0, 10) }}</span>
              </p>
            </button>
          </div>
          <p
            v-if="(mode === 'stashes' ? stashes : history).length === 0"
            class="px-2 py-1.5 text-base text-faint"
          >
            {{ mode === "stashes" ? "暂无 stash" : "暂无提交" }}
          </p>
          <p
            v-if="mode === 'history' && history.length === 100"
            class="px-2 py-1.5 text-base text-faint"
          >
            显示最近 100 条提交
          </p>
        </template>
      </div>
      <footer
        v-if="status"
        class="flex shrink-0 items-center justify-between gap-2 border-t border-line-soft p-2 text-sm"
      >
        <span
          class="min-w-0 flex items-center gap-1.5 truncate"
          :title="`${status.root}${status.upstream ? ` → ${status.upstream}` : ''}`"
          ><i-lucide-git-branch class="h-3.5 w-3.5 shrink-0" /><span
            class="truncate"
            >{{ status.branch }}</span
          ><span v-if="status.behind" class="text-faint"
            >↓{{ status.behind }}</span
          ></span
        >
        <div class="flex shrink-0 items-center border border-line-soft rounded">
          <button
            class="control-ghost px-2 py-1 disabled:opacity-40"
            :disabled="
              busy ||
              status.remotes.length === 0 ||
              status.branch === 'Detached HEAD'
            "
            @click="run('push')"
          >
            ↑{{ status.ahead || "" }} Push
          </button>
          <GitMenu
            label="Remote actions"
            side="top"
            :disabled="busy || status.remotes.length === 0"
            :groups="remoteMenus"
            @select="selectAction"
          />
        </div>
      </footer>
    </template>
    <AlertDialogRoot v-model:open="confirmationOpen">
      <AlertDialogPortal
        ><AlertDialogOverlay
          class="fixed inset-0 z-50 bg-black/40"
        /><AlertDialogContent
          class="fixed left-1/2 top-1/2 z-50 w-[min(420px,90vw)] border border-line-soft rounded-xl bg-surface p-5 shadow-[var(--oct-shadow)] -translate-x-1/2 -translate-y-1/2"
        >
          <AlertDialogTitle class="text-lg font-semibold">{{
            pending?.label
          }}</AlertDialogTitle>
          <AlertDialogDescription
            class="mt-3 whitespace-pre-wrap text-base text-dim"
            >{{ pending?.description }}</AlertDialogDescription
          >
          <div class="mt-5 flex justify-end gap-2">
            <AlertDialogCancel class="control-ghost rounded px-3 py-1.5"
              >取消</AlertDialogCancel
            ><AlertDialogAction
              class="rounded bg-danger-9 px-3 py-1.5 text-white"
              @click="confirmAction"
              >确认</AlertDialogAction
            >
          </div>
        </AlertDialogContent></AlertDialogPortal
      >
    </AlertDialogRoot>
  </aside>
</template>
<script setup lang="ts">
import { useLocalStorage } from "@vueuse/core";
import {
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogOverlay,
  AlertDialogPortal,
  AlertDialogRoot,
  AlertDialogTitle,
} from "reka-ui";
import { computed, ref, toRef, watch } from "vue";
import ChangesFileList from "@/components/changes/ChangesFileList.vue";
import GitMenu from "@/components/changes/GitMenu.vue";
import { useGit } from "@/composables/useGit";
import type { MenuItem } from "@/components/changes/GitMenu.vue";
import type { DiffSelection, GitAction } from "@/composables/useGit";

const props = defineProps<{ visible: boolean; projectId: string | null }>();
const emit = defineEmits<{ close: []; openDiff: [selection: DiffSelection] }>();
const { status, history, stashes, error, busy, loading, refresh, run } = useGit(
  toRef(props, "projectId"),
);
const mode = ref("changes");
const selected = ref<string>();
const view = useLocalStorage("oct-changes-view", "tree");
const grouping = useLocalStorage("oct-changes-grouping", "tracked");
const confirmationOpen = ref(false);
const pending = ref<{
  action: GitAction;
  label: string;
  description: string;
} | null>(null);
watch(
  () => props.projectId,
  () => {
    pending.value = null;
    confirmationOpen.value = false;
    selected.value = undefined;
    mode.value = "changes";
  },
);
const hasUnstaged = computed(
  () =>
    status.value?.files.some((f) => f.untracked || f.worktree !== " ") ?? false,
);
const totals = computed(() =>
  (status.value?.files ?? []).reduce(
    (sum, f) => ({
      additions: sum.additions + f.additions,
      deletions: sum.deletions + f.deletions,
    }),
    { additions: 0, deletions: 0 },
  ),
);
const viewMenus = computed<MenuItem[][]>(() => [
  [
    { id: "list", label: "List", checked: view.value === "list" },
    { id: "tree", label: "Tree", checked: view.value === "tree" },
  ],
  [
    { id: "none", label: "Group By: None", checked: grouping.value === "none" },
    {
      id: "tracked",
      label: "Tracked & Untracked",
      checked: grouping.value === "tracked",
    },
    {
      id: "staged",
      label: "Staged & Unstaged",
      checked: grouping.value === "staged",
    },
  ],
]);
const actionMenus = computed<MenuItem[][]>(() => {
  const files = status.value?.files ?? [];
  const tracked = files.some((f) => !f.untracked);
  return [
    [
      { id: "stage", label: "Stage All", disabled: !hasUnstaged.value },
      {
        id: "unstage",
        label: "Unstage All",
        disabled: !files.some((f) => !f.untracked && f.index !== " "),
      },
      { id: "restore", label: "Restore All Changes", disabled: !tracked },
    ],
    [
      { id: "stash_all", label: "Stash All", disabled: files.length === 0 },
      { id: "stash_tracked", label: "Stash Tracked", disabled: !tracked },
      {
        id: "stash_pop",
        label: "Stash Pop",
        disabled: stashes.value.length === 0,
      },
      {
        id: "view_stash",
        label: "View Stash",
        disabled: stashes.value.length === 0,
      },
    ],
    [
      {
        id: "discard_tracked",
        label: "Discard Tracked Changes",
        disabled: !files.some((f) => !f.untracked && f.worktree !== " "),
      },
      {
        id: "trash_untracked",
        label: "Trash Untracked Files",
        disabled: !files.some((f) => f.untracked),
      },
    ],
  ];
});
const remoteMenus = computed<MenuItem[][]>(() => [
  [
    { id: "fetch", label: "Fetch" },
    {
      id: "fetch_from",
      label: "Fetch From",
      children: status.value?.remotes.map((r) => ({
        id: `fetch:${r}`,
        label: r,
      })),
    },
    { id: "pull", label: "Pull (Fast-forward)" },
    { id: "pull_rebase", label: "Pull (Rebase)" },
  ],
  [
    { id: "push", label: "Push" },
    {
      id: "push_to",
      label: "Push To",
      children: status.value?.remotes.map((r) => ({
        id: `push:${r}`,
        label: r,
      })),
    },
    { id: "force_push", label: "Force Push (with lease)" },
  ],
]);
function setView(id: string) {
  if (id === "list" || id === "tree") view.value = id;
  else grouping.value = id;
}
function openDiff(selection: DiffSelection) {
  selected.value = selection.path;
  emit("openDiff", selection);
}
function selectAction(id: string) {
  if (id === "view_stash") {
    mode.value = "stashes";
    return;
  }
  const confirmations: Record<string, { label: string; description: string }> =
    {
      restore: {
        label: "Restore All Changes",
        description:
          "将所有已跟踪文件的暂存区和工作区恢复到 HEAD。未提交的修改会丢失；未跟踪文件保持不变。",
      },
      discard_tracked: {
        label: "Discard Tracked Changes",
        description:
          "将已跟踪文件的工作区恢复到暂存区版本，未暂存的修改会丢失。",
      },
      trash_untracked: {
        label: "Trash Untracked Files",
        description:
          "将全部未跟踪文件移入系统回收站。被 Git 忽略的文件保持不变。",
      },
      force_push: {
        label: "Force Push",
        description:
          "使用 --force-with-lease 更新远端分支，可能替换远端提交历史。",
      },
    };
  if (confirmations[id]) {
    pending.value = { action: id as GitAction, ...confirmations[id] };
    confirmationOpen.value = true;
    return;
  }
  const [action, ...remote] = id.split(":");
  void run(
    action as GitAction,
    undefined,
    remote.length > 0 ? remote.join(":") : undefined,
  );
}
function confirmAction() {
  if (pending.value) void run(pending.value.action);
  pending.value = null;
}
</script>
