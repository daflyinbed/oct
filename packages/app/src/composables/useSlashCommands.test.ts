import { describe, expect, it } from "vitest";
import {
  buildSlashMenuGroups,
  filterSlashMenuGroups,
  flattenMenuItems,
  formatPromptMessages,
  replaceSlashCommand,
  slashQueryOf,
} from "@/composables/useSlashCommands";
import type { components } from "@/api/schema";

type McpServerResponse = components["schemas"]["McpServerResponse"];

const T0 = "2026-09-22T00:00:00";

function mcpServer(
  id: string,
  overrides: Partial<McpServerResponse> = {},
): McpServerResponse {
  return {
    id,
    name: id,
    transport: "stdio",
    command: "npx",
    args: null,
    env: null,
    url: null,
    headers: null,
    enabled: true,
    tool_timeout_ms: null,
    project_id: null,
    created_at: T0,
    updated_at: T0,
    status: "connected",
    tool_count: 1,
    error: null,
    tools: [{ name: "t", description: null }],
    prompts: null,
    resources: null,
    ...overrides,
  };
}

const greet = {
  name: "greet",
  description: "greet someone",
  arguments: [{ name: "name", description: null, required: true }],
};
const review = {
  name: "review",
  description: "review the diff",
  arguments: [],
};

describe("buildSlashMenuGroups", () => {
  it("每个已连接且带 prompts 的 server 一组（按名排序），无 prompts 的不出现", () => {
    const groups = buildSlashMenuGroups([
      mcpServer("zeta", { prompts: [greet] }),
      mcpServer("alpha", { prompts: [review] }),
      mcpServer("empty", { prompts: [] }),
      mcpServer("no-cat", { prompts: null }),
      mcpServer("failed", {
        status: "failed",
        prompts: [greet],
        error: "spawn failed",
      }),
    ]);
    expect(groups.map((g) => g.label)).toEqual(["alpha", "zeta"]);
    expect(groups[0].items[0]).toMatchObject({
      kind: "prompt",
      serverId: "alpha",
      prompt: review,
    });
  });

  it("内置命令组在最前；无内置命令时不渲染命令组", () => {
    const builtin = {
      label: "compact",
      description: "压缩上下文",
      run: async () => null,
    };
    const withBuiltin = buildSlashMenuGroups(
      [mcpServer("s", { prompts: [greet] })],
      [builtin],
    );
    expect(withBuiltin.map((g) => g.label)).toEqual(["命令", "s"]);
    expect(withBuiltin[0].items[0]).toMatchObject({
      kind: "builtin",
      label: "compact",
    });
    expect(buildSlashMenuGroups([])).toEqual([]);
  });
});

describe("filterSlashMenuGroups", () => {
  const groups = buildSlashMenuGroups(
    [
      mcpServer("srv", {
        prompts: [greet, review],
      }),
    ],
    [
      {
        label: "compact",
        description: "压缩上下文",
        run: async () => null,
      },
    ],
  );

  it("空查询原样返回全部组", () => {
    expect(filterSlashMenuGroups(groups, "").length).toEqual(2);
  });

  it("按命令名/prompt 名/描述子串过滤（大小写不敏感），空组剔除", () => {
    expect(filterSlashMenuGroups(groups, "comp").map((g) => g.label)).toEqual([
      "命令",
    ]);
    expect(
      filterSlashMenuGroups(groups, "GREET").flatMap((g) => g.items),
    ).toHaveLength(1);
    // 描述命中也算（"review the diff"）。
    expect(
      filterSlashMenuGroups(groups, "diff").flatMap((g) => g.items),
    ).toHaveLength(1);
    expect(filterSlashMenuGroups(groups, "zzz")).toEqual([]);
  });

  it("flattenMenuItems 保持分组顺序的扁平序列", () => {
    expect(flattenMenuItems(groups).map((i) => i.key)).toEqual([
      "builtin:compact",
      "prompt:srv:greet",
      "prompt:srv:review",
    ]);
  });
});

describe("formatPromptMessages", () => {
  it("单条 user 消息直接用正文（无前缀）", () => {
    expect(formatPromptMessages([{ role: "user", text: "hi" }])).toBe("hi");
  });

  it("多 role 消息以 user:/assistant: 前缀、空行分隔拼接", () => {
    expect(
      formatPromptMessages([
        { role: "user", text: "Deploy to prod" },
        { role: "assistant", text: "Checklist ready" },
      ]),
    ).toBe("user: Deploy to prod\n\nassistant: Checklist ready");
  });
});

describe("slashQueryOf / replaceSlashCommand", () => {
  it("查询词取 / 后的首词；不以 / 开头返回 null", () => {
    expect(slashQueryOf("/")).toBe("");
    expect(slashQueryOf("/dep")).toBe("dep");
    // 空格与换行都算分界,其后的文本不属于命令词。
    expect(slashQueryOf("/deploy prod")).toBe("deploy");
    expect(slashQueryOf("/deploy\nplease")).toBe("deploy");
    expect(slashQueryOf("hello /deploy")).toBeNull();
    expect(slashQueryOf("")).toBeNull();
  });

  it("替换保留命令词之后的文本（含分界空白），光标语义由调用方处理", () => {
    expect(replaceSlashCommand("/deploy", "Deploy to prod")).toBe(
      "Deploy to prod",
    );
    expect(replaceSlashCommand("/deploy now\nplease", "Deploy to prod")).toBe(
      "Deploy to prod now\nplease",
    );
    // 非 / 开头的输入同样按首词替换(实际不会发生——菜单只在 / 开头时可选)。
    expect(replaceSlashCommand("plain text", "X")).toBe("X text");
  });
});
