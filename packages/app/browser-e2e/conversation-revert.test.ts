import { afterEach, expect, it } from "vitest";
import { page } from "vitest/browser";
import { mountApp } from "./support/app";
import {
  conversationFixture,
  finishEvent,
  projectFixture,
  providerFixture,
  storedMessage,
  textDelta,
} from "./support/fixtures";
import {
  chatText,
  COMPOSER_SELECTOR,
  sendChatMessage,
} from "./support/harness";
import { getRequests, resetMock, setScenario } from "./support/mock";
import type { MountedApp } from "./support/app";
import type { components } from "@/api/schema";

const BACKUP_NAME = /原会话（回退前）/;
let mounted: MountedApp | null = null;
afterEach(() => {
  mounted?.unmount();
  mounted = null;
});
const rows = [
  storedMessage("user", [{ type: "text", text: "第一问" }], {
    id: "u1",
    fork_cut: { kind: "message", message_id: "u1", side: "before" },
  }),
  storedMessage("assistant", [{ type: "text", text: "第一答" }], {
    id: "a1",
    fork_cut: { kind: "message", message_id: "a1", side: "after" },
  }),
  storedMessage("user", [{ type: "text", text: "待编辑的问题" }], {
    id: "u2",
    fork_cut: { kind: "message", message_id: "u2", side: "before" },
  }),
  storedMessage("assistant", [{ type: "text", text: "被撤销的回答" }], {
    id: "a2",
    fork_cut: { kind: "message", message_id: "a2", side: "after" },
  }),
];
const source = {
  ...conversationFixture("c1", "p1", "原会话"),
  history_epoch: 1,
};
const backup = {
  ...conversationFixture("backup", "p1", "原会话（回退前）"),
  origin_kind: "revert_backup",
  backup_of_conversation_id: "c1",
  backup_at_revision: 4,
};

async function boot(failure?: string) {
  await resetMock();
  const cut = {
    kind: "message",
    message_id: "u2",
    side: "before",
  } as const;
  const result: components["schemas"]["RevertResponse"] = {
    conversation: source,
    backup,
    backup_conversation_id: "backup",
    context_revision: 5,
    cut,
    draft: "待编辑的问题",
  };
  await setScenario({
    projects: [projectFixture()],
    conversations: [{ ...source, history_epoch: 0 }],
    providers: [providerFixture()],
    messages: { c1: rows },
    contextRevisions: { c1: 4 },
    revertReplies: {
      c1: failure
        ? { status: 409, code: failure, error: "blocked" }
        : {
            response: result,
            messages: rows.slice(0, 2),
            backupMessages: rows.map((row) => ({ ...row, inherited: true })),
          },
    },
    forkReplies: {
      backup: {
        response: {
          conversation: {
            ...conversationFixture("branch", "p1", "备份分支"),
            origin_kind: "fork",
            forked_at_revision: 4,
            forked_from_conversation_id: "backup",
          },
          context_revision: 4,
          cut: { kind: "head" },
          draft: null,
        },
        messages: rows.map((row) => ({ ...row, inherited: true })),
      },
    },
    scripts: {
      c1: [{ type: "events", events: [textDelta("重新回答"), finishEvent()] }],
    },
    afterStreamHistory: {
      c1: {
        revision: 7,
        messages: [
          ...rows.slice(0, 2),
          storedMessage("user", [{ type: "text", text: "修改后的问题" }]),
          storedMessage("assistant", [{ type: "text", text: "重新回答" }]),
        ],
      },
    },
  });
  mounted = await mountApp();
  await mounted.router.push("/conversation/c1");
  await expect.poll(chatText).toContain("被撤销的回答");
  // 右上角「分支当前会话」已移除；用户消息下的编辑按钮即回退入口
  await expect.element(page.getByTitle("回退并编辑").nth(1)).toBeEnabled();
}

it("用户消息回退保留原会话，回填草稿、创建只读备份并可从备份分支", async () => {
  await boot();
  await page.getByTitle("回退并编辑").nth(1).click();
  await expect
    .poll(async () => (await page.getByRole("dialog").element()).textContent)
    .toContain("隐藏 2 条消息");
  await expect
    .poll(async () => (await page.getByRole("dialog").element()).textContent)
    .toContain("仅回退对话；工作区文件保持当前状态");
  await page.getByRole("button", { name: "确认回退" }).click();
  await expect.poll(chatText).not.toContain("被撤销的回答");
  await expect.poll(chatText).toContain("第一答");
  expect(mounted!.router.currentRoute.value.path).toBe("/conversation/c1");
  await expect
    .element(page.getByPlaceholder(COMPOSER_SELECTOR))
    .toHaveValue("待编辑的问题");
  await expect
    .poll(() => document.querySelector("aside")?.textContent)
    .toContain("原会话（回退前）");
  const requests = await getRequests();
  expect(requests.find((r) => r.path.endsWith("/revert"))?.body).toMatchObject({
    expected_revision: 4,
    cut: { kind: "message", message_id: "u2", side: "before" },
  });
  expect(
    requests.some((r) => r.method === "POST" && r.path.endsWith("/messages")),
  ).toBe(false);
  await sendChatMessage("修改后的问题");
  await expect.poll(chatText).toContain("重新回答");
  const sent = (await getRequests()).find(
    (r) => r.method === "POST" && r.path.endsWith("/messages"),
  );
  expect(sent?.body).toMatchObject({ expected_revision: 5 });
  await mounted!.router.push("/conversation/backup");
  await expect.poll(chatText).toContain("回退前备份 · 只读");
  await expect.poll(chatText).toContain("被撤销的回答");
  await expect
    .element(page.getByPlaceholder(COMPOSER_SELECTOR))
    .not.toBeInTheDocument();
  // 只读备份里不提供回退编辑入口
  await expect.element(page.getByTitle("回退并编辑")).not.toBeInTheDocument();
  // 备份的唯一出路是分支：提示区的「分支后继续」按钮（head 切点）
  await page.getByRole("button", { name: "分支后继续" }).click();
  await expect
    .poll(() => mounted!.router.currentRoute.value.path)
    .toBe("/conversation/branch");
  await expect
    .element(page.getByPlaceholder(COMPOSER_SELECTOR))
    .toBeInTheDocument();
  await page
    .getByRole("complementary")
    .getByRole("button", { name: BACKUP_NAME })
    .click({ button: "right" });
  await page.getByRole("menuitem", { name: "删除备份" }).click();
  await expect
    .poll(() => document.querySelector("aside")?.textContent)
    .not.toContain("原会话（回退前）");
  expect(
    (await getRequests()).some(
      (r) => r.method === "DELETE" && r.path.endsWith("/backup"),
    ),
  ).toBe(true);
});

it.each([
  ["stale_revision", "刷新"],
  ["background_pending", "后台任务"],
])("%s 拒绝时保留当前历史，确认框显示原因", async (code, hint) => {
  await boot(code);
  await page.getByTitle("回退并编辑").nth(1).click();
  await page.getByRole("button", { name: "确认回退" }).click();
  await expect
    .poll(async () => (await page.getByRole("dialog").element()).textContent)
    .toContain(hint);
  expect(mounted!.router.currentRoute.value.path).toBe("/conversation/c1");
  expect(
    (await getRequests()).filter((r) => r.path.endsWith("/revert")),
  ).toHaveLength(1);
  await page.getByRole("button", { name: "取消", exact: true }).click();
  await expect.poll(chatText).toContain("被撤销的回答");
});
