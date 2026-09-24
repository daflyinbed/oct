import { isSubagentStatus } from "@/composables/useSubagents";
import type { SubagentStatus, SubagentTask } from "@/composables/useSubagents";

/**
 * TaskCard（父消息里的子代理任务卡片）的纯展示逻辑：从 ToolCallPart +
 * 任务 store 推导卡片状态。独立成模块便于单测（vitest node 环境不挂载
 * 组件）。
 */

/** task 工具调用参数的 description 字段；参数未生成完/损坏时为 null。 */
export function parseTaskDescription(argumentsJson: string): string | null {
  if (!argumentsJson) return null;
  try {
    const parsed: unknown = JSON.parse(argumentsJson);
    if (
      parsed !== null &&
      typeof parsed === "object" &&
      !Array.isArray(parsed)
    ) {
      const description = (parsed as Record<string, unknown>).description;
      if (typeof description === "string" && description.trim()) {
        return description.trim();
      }
    }
  } catch {
    // 半截 JSON（generating 中）或非 JSON：回退标题
  }
  return null;
}

/**
 * 结果摘要：content 是宿主生成的任务信封 JSON 时取其 output 字段头部，
 * 否则取原文头部（信封缺失/错误文本同样给用户可读的一截）。
 */
export function taskResultSummary(
  content: string | null,
  maxChars = 200,
): string | null {
  if (content == null) return null;
  let text = content;
  try {
    const parsed: unknown = JSON.parse(content);
    if (
      parsed !== null &&
      typeof parsed === "object" &&
      !Array.isArray(parsed) &&
      typeof (parsed as Record<string, unknown>).output === "string"
    ) {
      text = (parsed as Record<string, unknown>).output as string;
    }
  } catch {
    // 非 JSON 信封：原样取头部
  }
  const trimmed = text.trim();
  if (!trimmed) return null;
  return trimmed.length > maxChars ? `${trimmed.slice(0, maxChars)}…` : trimmed;
}

/** TaskCard 渲染所需的全部派生状态。 */
export interface TaskCardState {
  /** 任务描述（参数 description → store → 工具标题 → 兜底文案）。 */
  description: string;
  /** 任务状态：store（live）优先，其次结果 details，最后按卡片生命周期兜底。 */
  status: SubagentStatus;
  /** 子会话 id：运行中来自 store（subagent_updated 落地即可打开），终态来自 details。 */
  agentId: string | null;
  /** 当前活动摘要（正在执行的命令/读取的文件），仅运行中有。 */
  activity: string | null;
  /** 任务创建时间（store 有则用；纯 generating 卡片为 null → 不显示耗时）。 */
  createdAt: string | null;
  finishedAt: string | null;
}

/**
 * 推导卡片状态。数据源优先级：
 * 1. 任务 store（按父调用 call id 命中，live 权威——快照 + 事件实时维护）；
 * 2. tool_result 的 details（kind=subagent_task 信封元数据，历史回放路径）；
 * 3. 卡片自身生命周期兜底：interrupted → lost（待恢复）、generating/
 *    running → running（参数未生成完/已派发）、done 无元数据 → completed。
 */
function detailsDescription(
  details: Record<string, unknown> | null,
): string | null {
  const description = details?.description;
  return typeof description === "string" && description.trim()
    ? description.trim()
    : null;
}

export function resolveTaskCard(
  part: {
    title: string | null;
    arguments: string;
    status: string;
    details: Record<string, unknown> | null;
  },
  task: SubagentTask | null,
): TaskCardState {
  const description =
    parseTaskDescription(part.arguments) ??
    task?.description ??
    detailsDescription(part.details) ??
    part.title?.trim() ??
    "子代理任务";

  let status: SubagentStatus | null = task?.status ?? null;
  if (status == null && part.details) {
    const candidate = (part.details as Record<string, unknown>).status;
    if (isSubagentStatus(candidate)) status = candidate;
  }
  if (status == null) {
    if (part.status === "interrupted") status = "lost";
    else if (part.status === "generating" || part.status === "running") {
      // 参数生成中/已派发但尚无信封元数据：都视作进行前
      status = "running";
    } else status = "completed"; // done 且无元数据
  }

  const agentId =
    task?.agent_id ??
    (typeof part.details?.agent_id === "string"
      ? (part.details.agent_id as string)
      : null);

  return {
    description,
    status,
    agentId,
    activity: task?.activity ?? null,
    createdAt: task?.created_at ?? null,
    finishedAt: task?.finished_at ?? null,
  };
}
