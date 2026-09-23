import type { components } from "@/api/schema";

type McpServerResponse = components["schemas"]["McpServerResponse"];
type McpPromptSummary = components["schemas"]["McpPromptSummary"];

/**
 * 聊天输入框的通用斜杠命令机制：`/` 触发菜单，内置命令与 MCP prompts
 * （按 server 分组）并列展示。
 *
 * 内置命令在此注册（`builtinSlashCommands`）。需要会话上下文的命令
 * （如 `/compact`，要 emit 到页面层）在 ChatPanel 就地注册拼进首组；
 * 全局命令加进下面的注册表即可，菜单机制无需再动。
 */

/** 内置斜杠命令（非 MCP 来源），选中后直接执行 `run`。 */
export interface BuiltinSlashCommand {
  /** 展示名（不含 `/`），如 `compact`。 */
  label: string;
  description: string;
  /** 选中即执行；返回填入输入框的文本（null = 不填入，命令自行处理）。 */
  run: () => Promise<string | null>;
}

/**
 * 内置命令注册表（菜单首组「命令」）。全局命令在此追加；`/compact` 因
 * 需要会话上下文注册在 ChatPanel。
 */
export const builtinSlashCommands: readonly BuiltinSlashCommand[] = [];

/** 菜单项：内置命令或某个 server 的一个 prompt。 */
export type SlashMenuItem =
  | {
      kind: "builtin";
      key: string;
      label: string;
      description: string;
      command: BuiltinSlashCommand;
    }
  | {
      kind: "prompt";
      key: string;
      /** server 名（分组标签 + 前缀展示）。 */
      serverId: string;
      serverName: string;
      prompt: McpPromptSummary;
    };

/** 菜单的一个分组：内置命令组在前，之后每个 server 一组（按名排序）。 */
export interface SlashMenuGroup {
  label: string;
  items: SlashMenuItem[];
}

/**
 * 由内置命令 + 当前有效 MCP server 集合（已连接且带 prompts 目录）构建
 * 菜单分组。空组剔除；`servers` 即 useMcpStatus 的有效集合。
 */
export function buildSlashMenuGroups(
  servers: readonly McpServerResponse[],
  builtins: readonly BuiltinSlashCommand[] = [],
): SlashMenuGroup[] {
  const groups: SlashMenuGroup[] = [];
  if (builtins.length > 0) {
    groups.push({
      label: "命令",
      items: builtins.map((command) => ({
        kind: "builtin" as const,
        key: `builtin:${command.label}`,
        label: command.label,
        description: command.description,
        command,
      })),
    });
  }
  const connected = servers
    .filter(
      (server) =>
        server.status === "connected" && (server.prompts?.length ?? 0) > 0,
    )
    .sort((a, b) => a.name.localeCompare(b.name));
  for (const server of connected) {
    groups.push({
      label: server.name,
      items: (server.prompts ?? []).map((prompt) => ({
        kind: "prompt" as const,
        key: `prompt:${server.id}:${prompt.name}`,
        serverId: server.id,
        serverName: server.name,
        prompt,
      })),
    });
  }
  return groups;
}

/**
 * 按查询词过滤分组：匹配项的 label（内置命令）或 prompt 名/描述命中
 * 子串（大小写不敏感）即保留；空组剔除。查询词是输入框 `/` 后的首词。
 */
export function filterSlashMenuGroups(
  groups: readonly SlashMenuGroup[],
  query: string,
): SlashMenuGroup[] {
  const q = query.trim().toLowerCase();
  if (!q) return [...groups];
  return groups
    .map((group) => ({
      label: group.label,
      items: group.items.filter((item) => {
        const haystack =
          item.kind === "builtin"
            ? `${item.label} ${item.description}`
            : `${item.prompt.name} ${item.prompt.description ?? ""}`;
        return haystack.toLowerCase().includes(q);
      }),
    }))
    .filter((group) => group.items.length > 0);
}

/** 过滤后分组的扁平索引序列，供键盘上下移动。 */
export function flattenMenuItems(
  groups: readonly SlashMenuGroup[],
): SlashMenuItem[] {
  return groups.flatMap((group) => group.items);
}

/**
 * 把渲染结果（多 role messages）拼成一段填入输入框的文本：单条 user
 * 消息直接用正文；否则每条以 `user:`/`assistant:` 前缀标注、空行分隔。
 */
export function formatPromptMessages(
  messages: readonly { role: string; text: string }[],
): string {
  if (messages.length === 1 && messages[0].role === "user") {
    return messages[0].text;
  }
  return messages.map((m) => `${m.role}: ${m.text}`).join("\n\n");
}

/** 输入文本的首个空白字符（斜杠命令词与后续文本的分界）。 */
const RE_FIRST_WHITESPACE = /\s/;

/**
 * 输入框当前的斜杠查询词：输入以 `/` 开头时取首词去掉 `/`，否则 null
 * （菜单关闭）。多行输入同样以首个空白（含换行）分界。
 */
export function slashQueryOf(input: string): string | null {
  if (!input.startsWith("/")) return null;
  const firstWordEnd = input.search(RE_FIRST_WHITESPACE);
  return firstWordEnd === -1 ? input.slice(1) : input.slice(1, firstWordEnd);
}

/**
 * 用渲染结果替换输入里的命令首词（`/xxx` token），保留其后的文本
 * （含分界的空白字符），得到新的输入框内容。
 */
export function replaceSlashCommand(input: string, text: string): string {
  const firstWhitespace = input.search(RE_FIRST_WHITESPACE);
  const rest = firstWhitespace === -1 ? "" : input.slice(firstWhitespace);
  return `${text}${rest}`;
}
