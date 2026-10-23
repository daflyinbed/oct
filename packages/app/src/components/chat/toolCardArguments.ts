/** Tool argument parsing and display projection; no Vue state or IO. */
import { buildDiffRows, splitLines } from "./toolDiff";
import type { DiffRow } from "./toolDiff";
import type { ToolCallPart } from "@/composables/chat/display";

type Arguments = Record<string, unknown> | null;

// --- 参数展示 ---------------------------------------------------------------

function prettyArguments(part: ToolCallPart): string {
  const raw = part.arguments;
  if (!raw) return "";
  try {
    return JSON.stringify(JSON.parse(raw), null, 2);
  } catch {
    return raw;
  }
}

export function parseToolArguments(part: ToolCallPart): Arguments {
  const raw = part.arguments;
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
}

function argStr(args: Arguments, key: string): string | null {
  const value = args?.[key];
  return typeof value === "string" ? value : null;
}

function argNum(args: Arguments, key: string): number | null {
  const value = args?.[key];
  return typeof value === "number" ? value : null;
}

// 文件路径类工具：标题拆成 文件名（强调）+ 目录（弱化）
export function toolFilePath(
  part: ToolCallPart,
  args: Arguments,
): { name: string; dir: string | null; full: string } | null {
  let path: string | null = null;
  switch (part.name) {
    case "edit_file":
      path = argStr(args, "file_path");
      break;
    case "write_file":
    case "read_file":
    case "list_dir":
      path = argStr(args, "path");
      break;
    case "glob":
      path = argStr(args, "pattern");
      break;
    default:
      return null;
  }
  path ??= part.title?.trim() || null;
  if (!path) return null;
  const cut = path.lastIndexOf("/");
  return cut < 0
    ? { name: path, dir: null, full: path }
    : { name: path.slice(cut + 1), dir: path.slice(0, cut + 1), full: path };
}

// --- 编辑/写入：Changes 面板风格的 diff --------------------------------------

// edit_file / write_file 展开后的 diff 行（路径已由标题展示，不再重复）
export function toolFileEdit(
  part: ToolCallPart,
  args: Arguments,
): { lines: DiffRow[] } | null {
  if (part.status === "generating") return null;
  const name = part.name;
  if (name !== "edit_file" && name !== "write_file") return null;
  if (!args) return null;
  const linesOf = (key: string): string[] | null => {
    const value = args?.[key];
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
}

// 编辑/写入头部的增删行数统计
export function toolDiffStat(
  part: ToolCallPart,
  args: Arguments,
): { added: number; removed: number } | null {
  if (part.status === "generating") return null;
  const name = part.name;
  if (name === "write_file") {
    const content = argStr(args, "content");
    if (content === null) return null;
    return { added: splitLines(content).length, removed: 0 };
  }
  if (name !== "edit_file") return null;
  const oldText = argStr(args, "old_string");
  const newText = argStr(args, "new_string");
  if (oldText === null || newText === null) return null;
  return {
    added: splitLines(newText).length,
    removed: splitLines(oldText).length,
  };
}

// 终端卡片：命令与 stdout/stderr 合并渲染成 shell 样式
export interface ToolCommand {
  command: string;
  cwd: string | null;
}

export function toolCommand(
  part: ToolCallPart,
  args: Arguments,
): ToolCommand | null {
  if (part.name !== "execute_command") return null;
  const command = argStr(args, "command");
  if (command === null) return null;
  return { command, cwd: argStr(args, "working_dir") };
}

// 按工具把参数渲染成更易读的自然形式（路径、说明），
// 未识别的工具或字段缺失时回退为完整 JSON。
export type ArgBlockKind = "code" | "note";
export interface ArgBlock {
  kind: ArgBlockKind;
  text: string;
}

export function toolArgumentBlocks(
  part: ToolCallPart,
  args: Arguments,
): ArgBlock[] {
  if (!args) {
    // 半截/异常参数原样展示
    const raw = part.arguments;
    return raw ? [{ kind: "code", text: raw }] : [];
  }

  const blocks: ArgBlock[] = [];
  const notes: string[] = [];

  switch (part.name) {
    case "execute_code": {
      const code = argStr(args, "code");
      if (code !== null) blocks.push({ kind: "code", text: code });
      break;
    }
    case "read_file": {
      const path = argStr(args, "path");
      if (path === null) break;
      blocks.push({ kind: "code", text: path });
      const start = argNum(args, "start_line");
      const end = argNum(args, "end_line");
      if (start !== null && end !== null) notes.push(`第 ${start} - ${end} 行`);
      else if (start !== null) notes.push(`自第 ${start} 行起`);
      else if (end !== null) notes.push(`至第 ${end} 行`);
      break;
    }
    case "list_dir": {
      blocks.push({ kind: "code", text: argStr(args, "path") ?? "." });
      const depth = argNum(args, "depth");
      if (depth !== null) notes.push(`递归深度 ${depth}`);
      break;
    }
    case "grep": {
      const pattern = argStr(args, "pattern");
      if (pattern === null) break;
      blocks.push({ kind: "code", text: `/${pattern}/` });
      const path = argStr(args, "path");
      if (path) notes.push(`范围 ${path}`);
      const glob = argStr(args, "glob");
      if (glob) notes.push(`仅 ${glob}`);
      if (args.case_insensitive === true) notes.push("忽略大小写");
      const mode = argStr(args, "output_mode");
      if (mode === "files_with_matches") notes.push("仅列出文件");
      else if (mode === "count") notes.push("计数模式");
      const before = argNum(args, "before_context");
      const after = argNum(args, "after_context");
      if (before !== null && after !== null)
        notes.push(`上下文 -${before} / +${after} 行`);
      else if (before !== null) notes.push(`前文 ${before} 行`);
      else if (after !== null) notes.push(`后文 ${after} 行`);
      break;
    }
    case "glob": {
      const pattern = argStr(args, "pattern");
      if (pattern === null) break;
      blocks.push({ kind: "code", text: pattern });
      const path = argStr(args, "path");
      if (path) notes.push(`范围 ${path}`);
      const limit = argNum(args, "limit");
      if (limit !== null) notes.push(`最多 ${limit} 个`);
      break;
    }
    default:
      break;
  }

  if (blocks.length === 0) {
    return [{ kind: "code", text: prettyArguments(part) }];
  }
  if (notes.length > 0) {
    blocks.push({ kind: "note", text: notes.join(" · ") });
  }
  return blocks;
}
