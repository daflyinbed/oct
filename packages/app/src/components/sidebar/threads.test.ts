import { describe, expect, it } from "vitest";
import {
  buildThreadTree,
  groupThreadsByUpdatedAt,
  relativeThreadTime,
} from "./threads";
import type { components } from "@/api/schema";

type Conversation = components["schemas"]["Conversation"];

let sequence = 0;
function makeConversation(overrides: Partial<Conversation> = {}): Conversation {
  sequence += 1;
  return {
    id: `conv-${sequence}`,
    project_id: "p1",
    title: `对话 ${sequence}`,
    title_source: "default",
    kind: "chat",
    origin_kind: "normal",
    history_epoch: 0,
    permission_mode: "default",
    created_at: "2026-10-12T00:00:00Z",
    updated_at: "2026-10-12T00:00:00Z",
    ...overrides,
  };
}

describe("buildThreadTree", () => {
  it("普通会话保持平铺顶层", () => {
    const a = makeConversation();
    const b = makeConversation();
    const tree = buildThreadTree([a, b]);
    expect(tree.map((node) => node.conv.id)).toEqual([a.id, b.id]);
    expect(tree[0]!.children).toEqual([]);
  });

  it("fork 嵌套到源会话下，备份嵌套到被回退会话下", () => {
    const source = makeConversation();
    const fork = makeConversation({
      origin_kind: "fork",
      forked_from_conversation_id: source.id,
    });
    const original = makeConversation();
    const backup = makeConversation({
      origin_kind: "revert_backup",
      backup_of_conversation_id: original.id,
    });
    const tree = buildThreadTree([fork, source, backup, original]);
    expect(tree.map((node) => node.conv.id)).toEqual([source.id, original.id]);
    expect(tree[0]!.children.map((node) => node.conv.id)).toEqual([fork.id]);
    expect(tree[1]!.children.map((node) => node.conv.id)).toEqual([backup.id]);
  });

  it("源会话不在列表（被删除 SET NULL 或未加载）时退回顶层", () => {
    const orphanFork = makeConversation({
      origin_kind: "fork",
      // 指向同列表不存在的 id：后端 SET NULL 后该字段为 null，这里模拟
      // 未对账的中间态，同样必须兜底
      forked_from_conversation_id: "conv-missing",
    });
    const orphanBackup = makeConversation({
      origin_kind: "revert_backup",
      backup_of_conversation_id: null,
    });
    const tree = buildThreadTree([orphanFork, orphanBackup]);
    expect(tree.map((node) => node.conv.id)).toEqual([
      orphanFork.id,
      orphanBackup.id,
    ]);
  });

  it("fork 的 fork 递归嵌套到第二层", () => {
    const root = makeConversation();
    const fork = makeConversation({
      origin_kind: "fork",
      forked_from_conversation_id: root.id,
    });
    const forkOfFork = makeConversation({
      origin_kind: "fork",
      forked_from_conversation_id: fork.id,
    });
    const tree = buildThreadTree([forkOfFork, fork, root]);
    expect(tree.length).toBe(1);
    const [rootNode] = tree;
    expect(rootNode!.children[0]!.conv.id).toBe(fork.id);
    expect(rootNode!.children[0]!.children[0]!.conv.id).toBe(forkOfFork.id);
  });
});

describe("relativeThreadTime", () => {
  const now = Date.UTC(2026, 9, 12, 12, 0, 0);

  it.each([
    [2 * 60_000, "2m"],
    [3 * 3_600_000, "3h"],
    [1 * 86_400_000, "1d"],
    [4 * 30 * 86_400_000, "4mo"],
    [2 * 365 * 86_400_000, "2y"],
  ])("%i ms 前 → %s", (offset, expected) => {
    expect(relativeThreadTime(new Date(now - offset).toISOString(), now)).toBe(
      expected,
    );
  });

  it("未来时间戳（时钟偏差）按 1m 兜底", () => {
    expect(relativeThreadTime(new Date(now + 5_000).toISOString(), now)).toBe(
      "1m",
    );
  });
});

describe("groupThreadsByUpdatedAt", () => {
  // 用本地时间构造输入，避免测试依赖 CI 的时区设置
  const now = new Date(2026, 9, 12, 15, 0, 0).getTime();
  const at = (daysAgo: number, hour: number) =>
    new Date(2026, 9, 12 - daysAgo, hour, 0, 0).toISOString();
  const conversationAt = (daysAgo: number, hour: number) =>
    makeConversation({ updated_at: at(daysAgo, hour) });

  it("按日界分组：今天 / 昨天 / 星期X / 月份，组间按组内最新倒序", () => {
    const today = conversationAt(0, 9);
    const yesterday = conversationAt(1, 9);
    const threeDaysAgo = conversationAt(3, 9);
    const lastMonth = conversationAt(40, 9);
    const groups = groupThreadsByUpdatedAt(
      [lastMonth, threeDaysAgo, yesterday, today],
      now,
    );
    const weekday = `星期${["日", "一", "二", "三", "四", "五", "六"][new Date(2026, 9, 9).getDay()]}`;
    expect(groups.map((group) => group.label)).toEqual([
      "今天",
      "昨天",
      weekday,
      "9月",
    ]);
    expect(groups[0]!.conversations.map((c) => c.id)).toEqual([today.id]);
    expect(groups[2]!.conversations.map((c) => c.id)).toEqual([
      threeDaysAgo.id,
    ]);
  });

  it("同年往月显示「M月」，往年带年份「YYYY年M月」", () => {
    const groups = groupThreadsByUpdatedAt(
      [conversationAt(400, 9), conversationAt(40, 9)],
      now,
    );
    expect(groups.map((group) => group.label)).toEqual(["9月", "2025年9月"]);
  });

  it("同组多条保持传入顺序；未来时间戳并入今天", () => {
    const future = makeConversation({
      updated_at: new Date(now + 3_600_000).toISOString(),
    });
    const earlierToday = conversationAt(0, 9);
    const groups = groupThreadsByUpdatedAt([future, earlierToday], now);
    expect(groups.length).toBe(1);
    expect(groups[0]!.label).toBe("今天");
    expect(groups[0]!.conversations.map((c) => c.id)).toEqual([
      future.id,
      earlierToday.id,
    ]);
  });

  it("昨天边界：整 24 小时前但跨日界才算昨天，未跨日界算今天", () => {
    // now 是本地 15:00：今天 14:00 仍在今天，昨天 16:00 在昨天
    const groups = groupThreadsByUpdatedAt(
      [conversationAt(0, 14), conversationAt(1, 16)],
      now,
    );
    expect(groups.map((group) => group.label)).toEqual(["今天", "昨天"]);
  });
});
