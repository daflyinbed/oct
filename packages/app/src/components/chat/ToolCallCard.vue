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
        <span class="block h-full w-full" :class="iconClass" />
      </span>

      <span
        v-if="displayName"
        class="flex-none text-[12px]"
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
        class="min-w-0 flex-1 truncate text-[12px]"
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
        class="min-w-0 flex-1 truncate text-[12px] text-faint"
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
            class="text-[11px] text-success-10 leading-none font-mono"
            >+{{ diffStat.added }}</span
          >
          <span
            v-if="diffStat.removed > 0"
            class="text-[11px] text-danger-10 leading-none font-mono"
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
          class="h-[16px] inline-flex items-center whitespace-nowrap rounded-full bg-warning-7 px-1.5 text-[10.5px] text-warning-10 font-medium leading-none"
          >已中断</span
        >
        <!-- 已拒绝：权限门拒绝的调用（details kind=
             permission_denied），结构与「已清除」徽标同款 -->
        <span
          v-else-if="part.denied"
          class="h-[16px] inline-flex items-center whitespace-nowrap rounded-full bg-danger-7 px-1.5 text-[10.5px] text-danger-10 font-medium leading-none"
          >已拒绝</span
        >
        <!-- 已清除：微压缩把旧结果替换为占位符，原文可回查 -->
        <span
          v-else-if="part.cleared"
          class="h-[16px] inline-flex items-center whitespace-nowrap rounded-full bg-neutral-3 px-1.5 text-[10.5px] text-dim font-medium leading-none"
          >已清除</span
        >
        <template v-else-if="open">
          <span
            v-for="chip in chips"
            :key="chip.key"
            class="h-[16px] inline-flex items-center whitespace-nowrap rounded-full px-1.5 text-[10.5px] font-medium leading-none"
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
      <!-- 生成中：参数流式累积， pulsing cursor，不解析半截 JSON -->
      <div
        v-if="part.status === 'generating' && part.arguments"
        class="max-h-40 overflow-auto whitespace-pre-wrap break-all rounded-md bg-surface px-2.5 py-1.5 text-[11.5px] text-dim leading-[1.5] font-mono"
      >
        {{ part.arguments }}<span class="animate-pulse text-accent-7">▌</span>
      </div>

      <!-- 运行中/完成后：参数按工具自然展示 -->
      <template v-else>
        <!-- 终端：命令与 stdout/stderr 合并渲染成 shell 样式；运行中实时追加（用户在底部时才自动跟随滚动） -->
        <div
          v-if="terminal"
          ref="liveEl"
          class="max-h-72 overflow-auto whitespace-pre-wrap break-words rounded-md bg-surface px-2.5 py-1.5 text-[11.5px] text-neutral-10 leading-[1.55] font-mono"
          @scroll="onLiveScroll"
        >
          <span
            v-if="terminal.cwd"
            class="text-accent-10"
            :title="terminal.cwd"
            >{{ shortCwd(terminal.cwd) }}</span
          ><span class="text-faint">$ </span>{{ terminal.command
          }}<template
            v-if="
              part.status === 'running' ||
              (part.status === 'done' && part.content === null)
            "
            ><span
              v-for="(seg, i) in part.liveOutput.segments"
              :key="i"
              :class="
                seg.stream === 'stderr' ? 'text-warning-10' : 'text-neutral-10'
              "
              >{{ `\n${seg.text}` }}</span
            ></template
          ><span
            v-if="part.status === 'done' && part.content && !part.cleared"
            :class="part.isError ? 'text-danger-10' : ''"
            >{{ `\n${part.content}` }}</span
          >
        </div>

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
            <div v-if="block.kind === 'note'" class="text-[11px] text-faint">
              {{ block.text }}
            </div>
            <pre
              v-else
              class="max-h-72 overflow-auto whitespace-pre-wrap break-words rounded-md px-2.5 py-1.5 text-[11.5px] leading-[1.55] font-mono"
              :class="blockClassOf(block.kind)"
              >{{ block.text }}</pre
            >
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
              <div class="text-[11px] text-faint">
                该结果已从模型上下文清除（微压缩）
              </div>
              <button
                v-if="originalText === null && conversationId"
                type="button"
                class="text-[11px] text-accent-10 control-focus hover:underline disabled:opacity-60"
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
              class="max-h-72 overflow-auto whitespace-pre-wrap break-words rounded-md bg-surface px-2.5 py-1.5 text-[11.5px] text-neutral-10 leading-[1.55] font-mono"
              >{{ originalText }}</pre
            >
            <div v-else class="text-[11px] text-faint font-mono">
              {{ part.content }}
            </div>
          </template>
          <template v-else-if="!fileEdit && !terminal">
            <div class="mb-1 text-[11px] text-faint">返回给模型</div>
            <pre
              class="max-h-72 overflow-auto whitespace-pre-wrap break-words rounded-md px-2.5 py-1.5 text-[11.5px] leading-[1.55] font-mono"
              :class="
                part.isError
                  ? 'bg-danger-1 text-danger-10'
                  : 'bg-surface text-neutral-10'
              "
              >{{ part.content }}</pre
            >
            <p v-if="isTruncated" class="mt-1 text-[11px] text-faint">
              此内容已返回给模型（含截断）
            </p>
          </template>
        </div>
      </template>
    </div>
  </div>
</template>

<script setup lang="ts">
import { buildDiffRows, splitLines, type DiffRow } from "./toolDiff";
import { computed, nextTick, ref, watch } from "vue";
import client from "@/api/client";
import DiffLines from "@/components/diff/DiffLines.vue";
import BashTaskCard from "./BashTaskCard.vue";
import type { ToolCallPart } from "@/composables/useChat";

const props = defineProps<{
  part: ToolCallPart;
  /** 「查看原文」的回查作用域（microcompact 副本按会话 membership 校验）。 */
  conversationId: string | null;
}>();

// 折叠/展开（默认折叠）
const open = ref(false);

// --- 后台任务信封识别 --------------------------------------------------------
// live 路径 details 是 tool_result 事件的原始 JSON（kind 在顶层）；历史
// 路径 details 是 DTO 透传的 extra（dto 层把未识别 kind 原样放进 extra，
// useChat 已把它整体赋给 details），两处形状一致。
const backgroundTaskId = computed<string | null>(() => {
  if (props.part.name !== "execute_command") return null;
  const details = props.part.details;
  if (!details || details.kind !== "background_task") return null;
  const taskId = details.task_id;
  return typeof taskId === "string" && taskId ? taskId : null;
});

// --- 微压缩「查看原文」 ------------------------------------------------------

const originalText = ref<string | null>(null);
const loadingOriginal = ref(false);
const originalFailed = ref(false);

/** 经副本 id 回查被 supersedes 的原文（一次加载，失败可重试）。 */
async function loadOriginal(): Promise<void> {
  const copyId = props.part.clearedCopyMessageId;
  if (
    !copyId ||
    !props.conversationId ||
    loadingOriginal.value ||
    originalText.value !== null
  ) {
    return;
  }
  loadingOriginal.value = true;
  originalFailed.value = false;
  try {
    const { data, error } = await client.GET(
      "/api/conversations/{id}/messages/{copy_id}/original",
      {
        params: {
          path: { id: props.conversationId, copy_id: copyId },
        },
      },
    );
    if (error || !data) {
      originalFailed.value = true;
      return;
    }
    // 类型化 parts（与历史端点同一 DTO）；非 tool_result 的部分跳过。
    const chunks: string[] = [];
    for (const part of data.parts) {
      if (part.type !== "tool_result") continue;
      chunks.push(
        typeof part.content === "string"
          ? part.content
          : JSON.stringify(part.content ?? "", null, 2),
      );
    }
    originalText.value = chunks.join("\n");
  } catch {
    // 网络层异常（fetch 拒绝）：与 HTTP 错误同路径，允许重试。
    originalFailed.value = true;
  } finally {
    loadingOriginal.value = false;
  }
}

// 图标按工具名映射（完整类名写在此处，UnoCSS 可从源码提取）
const iconClass = computed(() => {
  switch (props.part.name) {
    case "execute_command":
      return "i-lucide-terminal";
    case "read_file":
      return "i-lucide-file-text";
    case "write_file":
      return "i-lucide-file-pen";
    case "edit_file":
      return "i-lucide-file-pen-line";
    case "list_dir":
      return "i-lucide-folder";
    case "grep":
      return "i-lucide-text-search";
    case "glob":
      return "i-lucide-folder-search";
    default:
      return "i-lucide-wrench";
  }
});

// 工具显示名（未知工具回退到原始名；生成中 name 为 null 时显示骨架占位）
const displayName = computed<string | null>(() => {
  switch (props.part.name) {
    case "execute_command":
      return "终端";
    case "read_file":
      return "读取文件";
    case "write_file":
      return "写入文件";
    case "edit_file":
      return "编辑";
    case "list_dir":
      return "列出目录";
    case "grep":
      return "搜索";
    case "glob":
      return "匹配文件";
    default:
      return props.part.name;
  }
});

const titleText = computed(() => props.part.title?.trim() || null);

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

// --- details 徽标 -----------------------------------------------------------

type ChipTone = "success" | "danger" | "warning" | "neutral";
interface Chip {
  key: string;
  label: string;
  tone: ChipTone;
}

const toneClass: Record<ChipTone, string> = {
  success: "bg-success-7 text-success-10",
  danger: "bg-danger-7 text-danger-10",
  warning: "bg-warning-7 text-warning-10",
  neutral: "bg-neutral-2 text-dim",
};

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  if (ms < 60000) return `${(ms / 1000).toFixed(1)}s`;
  return `${Math.floor(ms / 60000)}m ${Math.floor((ms % 60000) / 1000)}s`;
}

const chips = computed<Chip[]>(() => {
  const d = props.part.details;
  if (!d || props.part.status !== "done") return [];
  const num = (key: string): number | null =>
    typeof d[key] === "number" ? (d[key] as number) : null;
  const flag = (key: string) => d[key] === true;

  const result: Chip[] = [];

  const shown = num("shown_lines");
  const total = num("total_lines");
  if (shown !== null && total !== null) {
    result.push({
      key: "lines",
      label: `${shown}/${total} 行`,
      tone: "neutral",
    });
  }
  const matched = num("matched");
  if (matched !== null) {
    result.push({
      key: "matched",
      label: `${matched} 个匹配`,
      tone: "neutral",
    });
  }
  const found = num("found");
  if (found !== null) {
    result.push({ key: "found", label: `${found} 个结果`, tone: "neutral" });
  }
  const bytes = num("bytes");
  if (bytes !== null) {
    result.push({ key: "bytes", label: formatBytes(bytes), tone: "neutral" });
  }
  const replacements = num("replacements");
  if (replacements !== null) {
    result.push({
      key: "replacements",
      label: `${replacements} 处替换`,
      tone: "neutral",
    });
  }

  const exitCode = num("exit_code");
  if (exitCode !== null) {
    result.push({
      key: "exit",
      label: `exit ${exitCode}`,
      tone: exitCode === 0 ? "success" : "danger",
    });
  }
  if (flag("timed_out")) {
    result.push({ key: "timed_out", label: "超时", tone: "warning" });
  }
  if (flag("created")) {
    result.push({ key: "created", label: "新建文件", tone: "success" });
  }
  const wallTime = num("wall_time_ms");
  if (wallTime !== null) {
    result.push({
      key: "wall",
      label: formatDuration(wallTime),
      tone: "neutral",
    });
  }
  if (flag("truncated")) {
    const original = num("original_bytes");
    result.push({
      key: "truncated",
      label:
        original !== null ? `已截断 · 原始 ${formatBytes(original)}` : "已截断",
      tone: "warning",
    });
  }
  return result;
});

const isTruncated = computed(() => props.part.details?.truncated === true);

// --- 参数展示 ---------------------------------------------------------------

const prettyArguments = computed(() => {
  const raw = props.part.arguments;
  if (!raw) return "";
  try {
    return JSON.stringify(JSON.parse(raw), null, 2);
  } catch {
    return raw;
  }
});

const parsedArgs = computed<Record<string, unknown> | null>(() => {
  const raw = props.part.arguments;
  if (!raw) return null;
  try {
    const parsed: unknown = JSON.parse(raw);
    if (
      parsed === null ||
      typeof parsed !== "object" ||
      Array.isArray(parsed)
    ) {
      return null;
    }
    return parsed as Record<string, unknown>;
  } catch {
    return null;
  }
});

function argStr(key: string): string | null {
  const value = parsedArgs.value?.[key];
  return typeof value === "string" ? value : null;
}

function argNum(key: string): number | null {
  const value = parsedArgs.value?.[key];
  return typeof value === "number" ? value : null;
}

// 文件路径类工具：标题拆成 文件名（强调）+ 目录（弱化）
const filePathInfo = computed<{
  name: string;
  dir: string | null;
  full: string;
} | null>(() => {
  let path: string | null = null;
  if (props.part.name === "edit_file") path = argStr("file_path");
  else if (props.part.name === "write_file" || props.part.name === "read_file")
    path = argStr("path");
  path ??= titleText.value;
  if (!path) return null;
  const cut = path.lastIndexOf("/");
  return cut < 0
    ? { name: path, dir: null, full: path }
    : { name: path.slice(cut + 1), dir: path.slice(0, cut + 1), full: path };
});

// --- 编辑/写入：Changes 面板风格的 diff --------------------------------------

// edit_file / write_file 展开后的 diff 行（路径已由标题展示，不再重复）
const fileEdit = computed<{ lines: DiffRow[] } | null>(() => {
  if (props.part.status === "generating") return null;
  const name = props.part.name;
  if (name !== "edit_file" && name !== "write_file") return null;
  if (!parsedArgs.value) return null;
  const linesOf = (key: string): string[] | null => {
    const value = parsedArgs.value?.[key];
    return typeof value === "string" ? splitLines(value) : null;
  };
  if (name === "write_file") {
    const newLines = linesOf("content");
    if (newLines === null) return null;
    return { lines: buildDiffRows([], newLines) };
  }
  const oldLines = linesOf("old_string");
  const newLines = linesOf("new_string");
  if (oldLines === null || newLines === null) return null;
  return { lines: buildDiffRows(oldLines, newLines) };
});

// 编辑/写入头部的增删行数统计
const diffStat = computed<{ added: number; removed: number } | null>(() => {
  if (props.part.status === "generating") return null;
  const name = props.part.name;
  if (name === "write_file") {
    const content = argStr("content");
    if (content === null) return null;
    return { added: splitLines(content).length, removed: 0 };
  }
  if (name !== "edit_file") return null;
  const oldText = argStr("old_string");
  const newText = argStr("new_string");
  if (oldText === null || newText === null) return null;
  return {
    added: splitLines(newText).length,
    removed: splitLines(oldText).length,
  };
});

// 终端卡片：命令与 stdout/stderr 合并渲染成 shell 样式
const terminal = computed<{ command: string; cwd: string | null } | null>(
  () => {
    if (props.part.name !== "execute_command") return null;
    const command = argStr("command");
    if (command === null) return null;
    return { command, cwd: argStr("working_dir") };
  },
);

// 按工具把参数渲染成更易读的自然形式（路径、说明），
// 未识别的工具或字段缺失时回退为完整 JSON。
type ArgBlockKind = "code" | "note";
interface ArgBlock {
  kind: ArgBlockKind;
  text: string;
}

const blockTone: Record<Exclude<ArgBlockKind, "note">, string> = {
  code: "bg-surface text-neutral-10",
};

function blockClassOf(kind: ArgBlockKind): string {
  if (kind === "note") return "";
  return `${blockTone[kind]} px-2.5`;
}

// 提示符里的 cwd 过长时只留尾部一段，完整路径由 hover（title）展示
const CWD_TAIL_CHARS = 28;

function shortCwd(cwd: string): string {
  return cwd.length > CWD_TAIL_CHARS ? `…${cwd.slice(-CWD_TAIL_CHARS)}` : cwd;
}

const argBlocks = computed<ArgBlock[]>(() => {
  const args = parsedArgs.value;
  if (!args) {
    // 半截/异常参数原样展示
    const raw = props.part.arguments;
    return raw ? [{ kind: "code", text: raw }] : [];
  }

  const blocks: ArgBlock[] = [];
  const notes: string[] = [];

  switch (props.part.name) {
    case "read_file": {
      const path = argStr("path");
      if (path === null) break;
      blocks.push({ kind: "code", text: path });
      const start = argNum("start_line");
      const end = argNum("end_line");
      if (start !== null && end !== null) notes.push(`第 ${start} - ${end} 行`);
      else if (start !== null) notes.push(`自第 ${start} 行起`);
      else if (end !== null) notes.push(`至第 ${end} 行`);
      break;
    }
    case "list_dir": {
      blocks.push({ kind: "code", text: argStr("path") ?? "." });
      const depth = argNum("depth");
      if (depth !== null) notes.push(`递归深度 ${depth}`);
      break;
    }
    case "grep": {
      const pattern = argStr("pattern");
      if (pattern === null) break;
      blocks.push({ kind: "code", text: `/${pattern}/` });
      const path = argStr("path");
      if (path) notes.push(`范围 ${path}`);
      const glob = argStr("glob");
      if (glob) notes.push(`仅 ${glob}`);
      if (args.case_insensitive === true) notes.push("忽略大小写");
      const mode = argStr("output_mode");
      if (mode === "files_with_matches") notes.push("仅列出文件");
      else if (mode === "count") notes.push("计数模式");
      const before = argNum("before_context");
      const after = argNum("after_context");
      if (before !== null && after !== null)
        notes.push(`上下文 -${before} / +${after} 行`);
      else if (before !== null) notes.push(`前文 ${before} 行`);
      else if (after !== null) notes.push(`后文 ${after} 行`);
      break;
    }
    case "glob": {
      const pattern = argStr("pattern");
      if (pattern === null) break;
      blocks.push({ kind: "code", text: pattern });
      const path = argStr("path");
      if (path) notes.push(`范围 ${path}`);
      const limit = argNum("limit");
      if (limit !== null) notes.push(`最多 ${limit} 个`);
      break;
    }
    default:
      break;
  }

  if (blocks.length === 0) {
    return [{ kind: "code", text: prettyArguments.value }];
  }
  if (notes.length > 0) {
    blocks.push({ kind: "note", text: notes.join(" · ") });
  }
  return blocks;
});

// --- 实时输出自动滚动（仅在用户本就位于底部时跟随） ---------------------------

const liveEl = ref<HTMLElement | null>(null);
const stickToBottom = ref(true);

// 活跃输出总字符数（含被裁掉的头部），作为跟随滚动的触发信号
const liveSize = computed(() => {
  let size = props.part.liveOutput.droppedChars;
  for (const seg of props.part.liveOutput.segments) size += seg.text.length;
  return size;
});

function onLiveScroll() {
  const el = liveEl.value;
  if (!el) return;
  stickToBottom.value = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
}

watch(liveSize, async () => {
  if (!stickToBottom.value) return;
  await nextTick();
  const el = liveEl.value;
  if (el) el.scrollTop = el.scrollHeight;
});
</script>
