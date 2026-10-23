<template>
  <!-- 后台任务信封：execute_command 以 run_in_background 调用且结果
       details.kind === "background_task" 时改走专属任务卡（命令在后台
       运行时里，此卡片展示任务状态而非命令输出）；part key 不变 -->
  <BashTaskCard
    v-if="backgroundTaskId"
    :part="part"
    :conversation-id="conversationId"
  />
  <!-- 工具调用：默认折叠为单行摘要（hover 出现箭头提示可点开），点开后显示完整卡片 -->
  <div v-else class="min-w-0" :class="frameClass">
    <!-- 头部：工具图标 + 显示名 + 标题 + 状态（生成中脉冲 / 运行中 spinner / 完成后徽标），点击切换折叠 -->
    <button
      type="button"
      class="group min-w-0 w-full flex items-center gap-2 text-left control-focus"
      :class="
        open
          ? 'px-3 py-1.5'
          : 'rounded-md px-1.5 py-1 transition-colors hover:bg-neutral-2'
      "
      :aria-expanded="open"
      @click="open = !open"
    >
      <span
        class="h-3.5 w-3.5 flex-none"
        :class="
          errorTone
            ? 'text-danger-10'
            : interrupted
              ? 'text-warning-10'
              : 'text-dim'
        "
      >
        <component :is="iconComponent" class="block h-full w-full" />
      </span>

      <span
        v-if="displayName"
        class="flex-none text-sm"
        :class="errorTone ? 'text-danger-10' : 'text-strong'"
        >{{ displayName }}</span
      >
      <span
        v-else
        class="h-3 w-16 flex-none animate-pulse rounded bg-neutral-3"
      />

      <!-- 文件类工具：文件名（强调）+ 目录（弱化）；其余工具沿用标题 -->
      <span
        v-if="filePathInfo"
        class="min-w-0 flex-1 truncate text-sm"
        :title="filePathInfo.full"
      >
        <span
          class="mr-1"
          :class="errorTone ? 'text-danger-10' : 'text-strong'"
          >{{ filePathInfo.name }}</span
        >
        <span class="text-faint">{{ filePathInfo.dir }}</span>
      </span>
      <span
        v-else-if="titleText"
        class="min-w-0 flex-1 truncate text-sm text-faint"
        :title="titleText"
      >
        {{ titleText }}
      </span>
      <span v-else class="flex-1" />

      <span class="flex flex-none items-center gap-1">
        <!-- 编辑/写入的增删行数统计 -->
        <template v-if="diffStat">
          <span
            v-if="diffStat.added > 0"
            class="text-xs text-success-10 leading-none font-mono"
            >+{{ diffStat.added }}</span
          >
          <span
            v-if="diffStat.removed > 0"
            class="text-xs text-danger-10 leading-none font-mono"
            >−{{ diffStat.removed }}</span
          >
        </template>
        <template v-if="part.status === 'generating'">
          <span
            class="block h-1.5 w-1.5 animate-pulse rounded-full bg-accent-7"
          />
        </template>
        <i-lucide-loader-circle
          v-else-if="part.status === 'running'"
          class="h-3.5 w-3.5 animate-spin text-accent-7"
        />
        <!-- 已中断：后端崩溃等原因未拿到结果的调用，提示可恢复此轮 -->
        <span
          v-else-if="interrupted"
          class="h-[16px] inline-flex items-center whitespace-nowrap rounded-full bg-warning-7 px-1.5 text-badge text-warning-10 font-medium leading-none"
          >已中断</span
        >
        <!-- 已拒绝：权限门拒绝的调用（details kind=
             permission_denied），结构与「已清除」徽标同款 -->
        <span
          v-else-if="part.denied"
          class="h-[16px] inline-flex items-center whitespace-nowrap rounded-full bg-danger-7 px-1.5 text-badge text-danger-10 font-medium leading-none"
          >已拒绝</span
        >
        <!-- 已清除：微压缩把旧结果替换为占位符，原文可回查 -->
        <span
          v-else-if="part.cleared"
          class="h-[16px] inline-flex items-center whitespace-nowrap rounded-full bg-neutral-3 px-1.5 text-badge text-dim font-medium leading-none"
          >已清除</span
        >
        <template v-else-if="open">
          <span
            v-for="chip in chips"
            :key="chip.key"
            class="h-[16px] inline-flex items-center whitespace-nowrap rounded-full px-1.5 text-badge font-medium leading-none"
            :class="toneClass[chip.tone]"
            >{{ chip.label }}</span
          >
        </template>
        <i-lucide-chevron-right
          class="h-3.5 w-3.5 flex-none text-faint transition-all duration-150"
          :class="
            open
              ? 'rotate-90 opacity-100'
              : 'opacity-0 group-hover:opacity-100 group-focus-visible:opacity-100'
          "
        />
      </span>
    </button>

    <!-- 展开后的完整内容 -->
    <div
      v-if="open"
      class="min-w-0 border-t px-3 py-2 space-y-2"
      :class="
        errorTone
          ? 'border-danger-6'
          : interrupted
            ? 'border-warning-6'
            : 'border-line-soft'
      "
    >
      <ToolNestedCalls
        v-if="nestedCalls.length > 0"
        :calls="nestedCalls"
        :running="part.status === 'running'"
      />
      <!-- 生成中：参数流式累积， pulsing cursor，不解析半截 JSON -->
      <div
        v-if="part.status === 'generating' && part.arguments"
        class="max-h-40 overflow-auto whitespace-pre-wrap break-all rounded-md bg-surface px-2.5 py-1.5 text-code text-dim font-mono"
      >
        {{ part.arguments }}<span class="animate-pulse text-accent-7">▌</span>
      </div>

      <!-- 运行中/完成后：参数按工具自然展示 -->
      <template v-else>
        <!-- 终端：命令与 stdout/stderr 合并渲染成 shell 样式；运行中实时追加（用户在底部时才自动跟随滚动） -->
        <ToolCommandOutput
          v-if="terminal"
          v-model:following="followOutput"
          :part="part"
          :command="terminal"
        />

        <!-- 文件编辑：复用 Changes 面板的 diff 展示 -->
        <div
          v-else-if="fileEdit"
          class="max-h-72 overflow-auto border border-line-soft rounded-md"
        >
          <DiffLines :lines="fileEdit.lines" />
        </div>

        <!-- 参数（按工具自然展示，未识别时回退 JSON） -->
        <div v-else-if="argBlocks.length > 0" class="min-w-0 space-y-1.5">
          <template v-for="(block, i) in argBlocks" :key="i">
            <div v-if="block.kind === 'note'" class="text-xs text-faint">
              {{ block.text }}
            </div>
            <pre
              v-else
              class="max-h-72 overflow-auto whitespace-pre-wrap break-words rounded-md px-2.5 py-1.5 text-code font-mono"
              :class="blockClassOf(block.kind)"
              >{{ block.text }}</pre>
          </template>
        </div>

        <!-- 结果区。微压缩已清除：content 是占位符，清除态块对终端/编辑类
             卡片同样渲染（命令/diff 仍在其上方），原文经副本 id 回查
             original 端点。编辑/写入的成功结果与终端输出（已并入上方命令
             块）无需重复展示；TODO: 编辑/写入失败（isError）时目前没有任何
             结果反馈，后续专门设计失败态的展示 -->
        <div v-if="part.status === 'done' && part.content !== null">
          <template v-if="part.cleared">
            <div class="mb-1 flex items-center justify-between gap-2">
              <div class="text-xs text-faint">
                该结果已从模型上下文清除（微压缩）
              </div>
              <button
                v-if="originalText === null && conversationId"
                type="button"
                class="text-xs text-accent-10 control-focus hover:underline disabled:opacity-60"
                :disabled="loadingOriginal"
                @click="loadOriginal"
              >
                {{
                  loadingOriginal
                    ? "加载中…"
                    : originalFailed
                      ? "加载失败，重试"
                      : "查看原文"
                }}
              </button>
            </div>
            <pre
              v-if="originalText !== null"
              class="max-h-72 overflow-auto whitespace-pre-wrap break-words rounded-md bg-surface px-2.5 py-1.5 text-code text-neutral-10 font-mono"
              >{{ originalText }}</pre>
            <div v-else class="text-xs text-faint font-mono">
              {{ part.content }}
            </div>
          </template>
          <template v-else-if="!fileEdit && !terminal">
            <div class="mb-1 text-xs text-faint">返回给模型</div>
            <pre
              class="max-h-72 overflow-auto whitespace-pre-wrap break-words rounded-md px-2.5 py-1.5 text-code font-mono"
              :class="
                part.isError
                  ? 'bg-danger-1 text-danger-10'
                  : 'bg-surface text-neutral-10'
              "
              >{{ part.content }}</pre>
            <p v-if="isTruncated" class="mt-1 text-xs text-faint">
              此内容已返回给模型（含截断）
            </p>
          </template>
        </div>
      </template>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed, ref } from "vue";
import IconFilePen from "~icons/lucide/file-pen";
import IconFilePenLine from "~icons/lucide/file-pen-line";
import IconFileText from "~icons/lucide/file-text";
import IconFolder from "~icons/lucide/folder";
import IconFolderSearch from "~icons/lucide/folder-search";
import IconTerminal from "~icons/lucide/terminal";
import IconTextSearch from "~icons/lucide/text-search";
import IconWrench from "~icons/lucide/wrench";
import DiffLines from "@/components/diff/DiffLines.vue";
import BashTaskCard from "./BashTaskCard.vue";
import {
  parseToolArguments,
  toolArgumentBlocks,
  toolCommand,
  toolDiffStat,
  toolFileEdit,
  toolFilePath,
} from "./toolCardArguments";
import {
  backgroundTaskId as getBackgroundTaskId,
  nestedToolCalls,
  toolDisplayName,
  toolResultChips,
} from "./toolCardMetadata";
import ToolCommandOutput from "./ToolCommandOutput.vue";
import ToolNestedCalls from "./ToolNestedCalls.vue";
import { useToolOriginal } from "./useToolOriginal";
import type { ArgBlockKind } from "./toolCardArguments";
import type { ChipTone } from "./toolCardMetadata";
import type { ToolCallPart } from "@/composables/chat/display";
import type { Component } from "vue";

const props = defineProps<{
  part: ToolCallPart;
  /** 「查看原文」的回查作用域（microcompact 副本按会话 membership 校验）。 */
  conversationId: string | null;
}>();

// 折叠/展开（默认折叠）
const open = ref(false);

const followOutput = ref(true);
const backgroundTaskId = computed(() => getBackgroundTaskId(props.part));
const { originalText, loadingOriginal, originalFailed, loadOriginal } =
  useToolOriginal(
    () => props.part,
    () => props.conversationId,
  );

// 图标按工具名映射（unplugin-icons 显式导入，<component :is> 渲染，颜色随外层 currentColor）
const iconComponent = computed<Component>(() => {
  switch (props.part.name) {
    case "execute_command":
      return IconTerminal;
    case "read_file":
      return IconFileText;
    case "write_file":
      return IconFilePen;
    case "edit_file":
      return IconFilePenLine;
    case "list_dir":
      return IconFolder;
    case "grep":
      return IconTextSearch;
    case "glob":
      return IconFolderSearch;
    default:
      return IconWrench;
  }
});

const displayName = computed(() => toolDisplayName(props.part));
const titleText = computed(() => props.part.title?.trim() || null);
const nestedCalls = computed(() => nestedToolCalls(props.part));
const errorTone = computed(
  () => props.part.status === "done" && props.part.isError,
);

const interrupted = computed(() => props.part.status === "interrupted");

// 折叠时无边框卡片；展开后为带边框的完整卡片（出错红边、中断黄边）
const frameClass = computed(() => {
  if (!open.value) return "";
  const border = errorTone.value
    ? "border-danger-6"
    : interrupted.value
      ? "border-warning-6"
      : "border-line-soft";
  return `rounded-[10px] border bg-panel ${border}`;
});

const toneClass: Record<ChipTone, string> = {
  success: "bg-success-7 text-success-10",
  danger: "bg-danger-7 text-danger-10",
  warning: "bg-warning-7 text-warning-10",
  neutral: "bg-neutral-2 text-dim",
};

const chips = computed(() => toolResultChips(props.part));
const isTruncated = computed(() => props.part.details?.truncated === true);
const parsedArgs = computed(() => parseToolArguments(props.part));
const filePathInfo = computed(() => toolFilePath(props.part, parsedArgs.value));
const fileEdit = computed(() => toolFileEdit(props.part, parsedArgs.value));
const diffStat = computed(() => toolDiffStat(props.part, parsedArgs.value));
const terminal = computed(() => toolCommand(props.part, parsedArgs.value));
const argBlocks = computed(() =>
  toolArgumentBlocks(props.part, parsedArgs.value),
);

const blockTone: Record<Exclude<ArgBlockKind, "note">, string> = {
  code: "bg-surface text-neutral-10",
};

function blockClassOf(kind: ArgBlockKind): string {
  if (kind === "note") return "";
  return `${blockTone[kind]} px-2.5`;
}
</script>
