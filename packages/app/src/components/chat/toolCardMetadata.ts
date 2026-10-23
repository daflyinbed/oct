/** Pure presentation normalization shared by the card and its child views. */
import type { ToolCallPart } from "@/composables/chat/display";

// --- 后台任务信封识别 --------------------------------------------------------
// live 路径 details 是 tool_result 事件的原始 JSON（kind 在顶层）；历史
// 路径 details 是 DTO 透传的 extra（dto 层把未识别 kind 原样放进 extra，
// useChat 已把它整体赋给 details），两处形状一致。
export function backgroundTaskId(part: ToolCallPart): string | null {
  if (part.name !== "execute_command") return null;
  const details = part.details;
  if (!details || details.kind !== "background_task") return null;
  const taskId = details.task_id;
  return typeof taskId === "string" && taskId ? taskId : null;
}

// 工具显示名（未知工具回退到原始名；生成中 name 为 null 时显示骨架占位）
export function toolDisplayName(part: ToolCallPart): string | null {
  switch (part.name) {
    case "execute_command":
      return "终端";
    case "execute_code":
      return "代码执行";
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
      return part.name;
  }
}

export interface NestedCall {
  id: string;
  name: string;
  title: string;
  status: string;
  error?: string | null;
  error_code?: string | null;
  duration_ms?: number | null;
}
export function nestedToolCalls(part: ToolCallPart): NestedCall[] {
  const calls = part.details?.calls;
  if (part.name !== "execute_code" || !Array.isArray(calls)) return [];
  return calls.filter(
    (row): row is NestedCall =>
      row &&
      typeof row === "object" &&
      typeof row.id === "string" &&
      typeof row.name === "string" &&
      typeof row.title === "string" &&
      typeof row.status === "string",
  );
}

// --- details 徽标 -----------------------------------------------------------

export type ChipTone = "success" | "danger" | "warning" | "neutral";
export interface Chip {
  key: string;
  label: string;
  tone: ChipTone;
}

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

export function toolResultChips(part: ToolCallPart): Chip[] {
  const d = part.details;
  if (!d || part.status !== "done") return [];
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
}
