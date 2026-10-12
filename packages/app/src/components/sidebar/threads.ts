import type { components } from "@/api/schema";

type Conversation = components["schemas"]["Conversation"];

/** 侧栏线程树节点：一行对话 + 挂在它下面的分叉/回退备份子树。 */
export interface ThreadNode {
  conv: Conversation;
  children: ThreadNode[];
}

/**
 * 把项目平铺的会话列表组装成线程树：fork 挂到 `forked_from_conversation_id`
 * 指向的会话下，回退备份挂到 `backup_of_conversation_id` 指向的会话下；
 * 源会话不在列表（已删除被 SET NULL、或尚未加载）时退回顶层。各层内保持
 * 传入顺序（后端按 updated_at DESC 返回）。fork 链不可能成环——fork/备份
 * 创建时源会话必须已存在，FK 保证因果方向。
 */
export function buildThreadTree(
  conversations: readonly Conversation[],
): ThreadNode[] {
  const ids = new Set(conversations.map((conversation) => conversation.id));
  const childrenByParent = new Map<string, Conversation[]>();
  const roots: Conversation[] = [];
  for (const conversation of conversations) {
    const parentId =
      conversation.forked_from_conversation_id ??
      conversation.backup_of_conversation_id ??
      null;
    if (parentId !== null && ids.has(parentId)) {
      const siblings = childrenByParent.get(parentId) ?? [];
      siblings.push(conversation);
      childrenByParent.set(parentId, siblings);
    } else {
      roots.push(conversation);
    }
  }
  const build = (conversation: Conversation): ThreadNode => ({
    conv: conversation,
    children: (childrenByParent.get(conversation.id) ?? []).map(build),
  });
  return roots.map(build);
}

/**
 * 侧栏线程行的相对时间短标（redesign demo 的 t-time：2m/2h/1d/4mo/3y）。
 * 未来时间戳（时钟偏差）按 1m 兜底，不出负数。
 */
export function relativeThreadTime(iso: string, now: number = Date.now()) {
  const diff = Math.max(0, now - new Date(iso).getTime());
  const minute = 60_000;
  const hour = 60 * minute;
  const day = 24 * hour;
  const month = 30 * day;
  const year = 365 * day;
  if (diff < hour) return `${Math.max(1, Math.round(diff / minute))}m`;
  if (diff < day) return `${Math.round(diff / hour)}h`;
  if (diff < month) return `${Math.round(diff / day)}d`;
  if (diff < year) return `${Math.round(diff / month)}mo`;
  return `${Math.round(diff / year)}y`;
}

/** 按时间排序视图的日期分组：标签 + 组内会话（保持传入顺序）。 */
export interface ThreadTimeGroup {
  label: string;
  conversations: Conversation[];
}

const WEEKDAY_LABELS = ["日", "一", "二", "三", "四", "五", "六"] as const;

/**
 * 把平铺会话按最后更新时间（updated_at，本地时区日界）分组成
 * 今天 / 昨天 / 星期X（2–6 天前）/ 月份（同年「M月」、往年「YYYY年M月」）。
 * 组间按组内最新更新时间倒序；组内保持传入顺序——列表本就按 updated_at
 * DESC 返回。未来时间戳（时钟偏差）并入今天。
 */
export function groupThreadsByUpdatedAt(
  conversations: readonly Conversation[],
  now: number = Date.now(),
): ThreadTimeGroup[] {
  const nowDate = new Date(now);
  const startOfDay = (time: number) => {
    const date = new Date(time);
    return new Date(
      date.getFullYear(),
      date.getMonth(),
      date.getDate(),
    ).getTime();
  };
  const todayStart = startOfDay(now);

  const groups = new Map<string, ThreadTimeGroup>();
  const groupOf = (key: string, label: string): ThreadTimeGroup => {
    let group = groups.get(key);
    if (!group) {
      group = { label, conversations: [] };
      groups.set(key, group);
    }
    return group;
  };

  for (const conversation of conversations) {
    const time = new Date(conversation.updated_at).getTime();
    const daysAgo = Math.round((todayStart - startOfDay(time)) / 86_400_000);
    let key: string;
    let label: string;
    if (daysAgo <= 0) {
      key = "today";
      label = "今天";
    } else if (daysAgo === 1) {
      key = "yesterday";
      label = "昨天";
    } else if (daysAgo <= 6) {
      key = `dow-${daysAgo}`;
      label = `星期${WEEKDAY_LABELS[new Date(time).getDay()]}`;
    } else {
      const date = new Date(time);
      key = `month-${date.getFullYear()}-${date.getMonth()}`;
      label =
        date.getFullYear() === nowDate.getFullYear()
          ? `${date.getMonth() + 1}月`
          : `${date.getFullYear()}年${date.getMonth() + 1}月`;
    }
    groupOf(key, label).conversations.push(conversation);
  }

  // 组间按组内最新 updated_at 倒序；组内保持传入顺序（列表本就按
  // updated_at DESC 返回，这里不依赖该前提重排，max 兜底乱序输入）。
  // 展开到命名数组后原地排序：不能写成 `[...groups.values()].sort(...)`——
  // e18e/prefer-array-to-sorted 的 autofix 会把「拷贝 + sort」机械重写成
  // `toSorted`，而 MapIterator 上没有这个方法（toSorted 只在 Array 上）。
  const latestUpdatedAt = (group: ThreadTimeGroup) =>
    Math.max(
      ...group.conversations.map((c) => new Date(c.updated_at).getTime()),
    );
  const groupList = [...groups.values()];
  groupList.sort((a, b) => latestUpdatedAt(b) - latestUpdatedAt(a));
  return groupList;
}
