import { afterEach, expect, it } from "vitest";
import { page, userEvent } from "vitest/browser";
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

async function boot(side: "before" | "after" = "before", failure?: string) {
  await resetMock();
  const cut = {
    kind: "message",
    message_id: side === "before" ? "u2" : "a1",
    side,
  } as const;
  const result: components["schemas"]["RevertResponse"] = {
    conversation: source,
    backup,
    backup_conversation_id: "backup",
    context_revision: 5,
    cut,
    draft: side === "before" ? "待编辑的问题" : null,
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
  await expect.element(page.getByTitle("分支当前会话")).toBeEnabled();
}

it("用户消息回退保留原会话，回填草稿、创建只读备份并可从备份分支", async () => {
  await boot();
  await page.getByTitle("消息操作").nth(2).click();
  await page.getByRole("menuitem", { name: "回退并编辑" }).click();
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
  await page.getByTitle("消息操作").nth(0).click();
  await expect
    .element(page.getByRole("menuitem", { name: "回退并编辑" }))
    .not.toBeInTheDocument();
  await userEvent.keyboard("{Escape}");
  await page.getByTitle("分支当前会话").click();
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

it("回复后回退保留该回复，输入框为空且不自动发送", async () => {
  await boot("after");
  await page.getByTitle("消息操作").nth(1).click();
  await page.getByRole("menuitem", { name: "回退到这条回复" }).click();
  await page.getByRole("button", { name: "确认回退" }).click();
  await expect.poll(chatText).toContain("第一答");
  await expect.poll(chatText).not.toContain("被撤销的回答");
  await expect
    .element(page.getByPlaceholder(COMPOSER_SELECTOR))
    .toHaveValue("");
  expect(
    (await getRequests()).some(
      (r) => r.method === "POST" && r.path.endsWith("/messages"),
    ),
  ).toBe(false);
});

it.each([
  ["stale_revision", "刷新"],
  ["background_pending", "后台任务"],
])("%s 拒绝时保留当前历史，确认框显示原因", async (code, hint) => {
  await boot("before", code);
  await page.getByTitle("消息操作").nth(2).click();
  await page.getByRole("menuitem", { name: "回退并编辑" }).click();
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
