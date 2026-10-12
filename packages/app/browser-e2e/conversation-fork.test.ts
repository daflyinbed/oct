import { afterEach, expect, it } from "vitest";
import { page } from "vitest/browser";
import { useProjects } from "@/composables/useProjects";
import { mountApp } from "./support/app";
import {
  conversationFixture,
  finishEvent,
  projectFixture,
  providerFixture,
  storedMessage,
  textDelta,
  toolCallStart,
} from "./support/fixtures";
import { chatText, sendChatMessage } from "./support/harness";
import { getRequests, resetMock, setScenario } from "./support/mock";
import type { MountedApp } from "./support/app";
import type { components } from "@/api/schema";

type ForkCut = components["schemas"]["ForkCut"];
const TERMINAL_CARD_HEADER = /终端/;
let mounted: MountedApp | null = null;
afterEach(() => {
  mounted?.unmount();
  mounted = null;
});

const rows = [
  storedMessage("user", [{ type: "text", text: "原问题" }], {
    id: "u1",
    fork_cut: { kind: "message", message_id: "u1", side: "before" },
  }),
  storedMessage("assistant", [{ type: "text", text: "原回复" }], {
    id: "a1",
    fork_cut: { kind: "message", message_id: "a1", side: "after" },
  }),
];

async function boot(cut: ForkCut, draft: string | null, inherited = rows) {
  await resetMock();
  const branch = {
    ...conversationFixture("branch", "p1", "原会话（分支）"),
    forked_from_conversation_id: "c1",
    forked_at_revision: inherited.length,
  };
  await setScenario({
    projects: [projectFixture()],
    conversations: [conversationFixture("c1", "p1", "原会话")],
    providers: [providerFixture()],
    messages: { c1: rows },
    contextRevisions: { c1: 2 },
    forkReplies: {
      c1: {
        response: {
          conversation: branch,
          context_revision: inherited.length,
          cut,
          draft,
        },
        messages: inherited.map((row) => ({ ...row, inherited: true })),
      },
    },
    scripts: {
      branch: [
        { type: "events", events: [textDelta("分支新回复"), finishEvent()] },
      ],
    },
    afterStreamHistory: {
      branch: {
        revision: inherited.length + 2,
        messages: [
          ...inherited.map((row) => ({ ...row, inherited: true })),
          storedMessage("user", [{ type: "text", text: "分支追问" }]),
          storedMessage("assistant", [{ type: "text", text: "分支新回复" }]),
        ],
      },
    },
  });
  mounted = await mountApp();
  await mounted.router.push("/conversation/c1");
  await expect.poll(chatText).toContain("原回复");
  // 右上角「分支当前会话」已移除，助手消息下有分支按钮即视为就绪
  await expect.element(page.getByTitle("从这条回复后分支")).toBeEnabled();
}

it("当前会话分支加入侧栏、展示来源，并能独立继续发送", async () => {
  const cut = {
    kind: "message",
    message_id: "a1",
    side: "after",
  } as const;
  await boot(cut, null);
  await page.getByTitle("从这条回复后分支").click();
  await expect
    .poll(() => mounted?.router.currentRoute.value.path)
    .toBe("/conversation/branch");
  await expect.poll(chatText).toContain("与来源共用工作区文件");
  await expect
    .element(page.getByRole("link", { name: "查看来源会话" }))
    .toBeInTheDocument();
  await expect
    .poll(() => document.querySelector("aside")?.textContent)
    .toContain("原会话（分支）");
  const beforeSend = await getRequests();
  expect(
    beforeSend.filter(
      (request) =>
        request.method === "POST" && request.path.endsWith("/messages"),
    ),
  ).toHaveLength(0);
  expect(
    beforeSend.find((request) => request.path.endsWith("/fork"))?.body,
  ).toMatchObject({ expected_revision: 2, cut });
  await sendChatMessage("分支追问");
  await expect.poll(chatText).toContain("分支新回复");
  await mounted!.router.push("/conversation/c1");
  await expect.poll(chatText).toContain("原回复");
  await expect.poll(chatText).not.toContain("分支新回复");
});

it("删除来源后开放的分支立即移除来源链接，并保留分支标识", async () => {
  await boot({ kind: "message", message_id: "a1", side: "after" }, null);
  await page.getByTitle("从这条回复后分支").click();
  await expect
    .poll(() => mounted?.router.currentRoute.value.path)
    .toBe("/conversation/branch");
  await expect
    .element(page.getByRole("link", { name: "查看来源会话" }))
    .toBeInTheDocument();

  // The sidebar has no delete control yet; exercise its public store operation.
  await useProjects().deleteConversation("p1", "c1");

  await expect
    .element(page.getByRole("link", { name: "查看来源会话" }))
    .not.toBeInTheDocument();
  await expect.poll(chatText).toContain("分支会话");
  await expect.poll(chatText).toContain("与来源共用工作区文件");
  await expect.poll(chatText).toContain("原回复");
  expect(mounted?.router.currentRoute.value.path).toBe("/conversation/branch");
  const requests = await getRequests();
  expect(
    requests.some(
      (request) =>
        request.method === "DELETE" &&
        request.path === "/api/projects/p1/conversations/c1",
    ),
  ).toBe(true);
});

it("回复后分支保留回复，继承的后台任务不带停止或查看输出按钮", async () => {
  const taskRows = [
    storedMessage("user", [{ type: "text", text: "运行命令" }]),
    storedMessage("assistant", [
      {
        type: "tool_call",
        id: "bg-call",
        name: "execute_command",
        arguments: '{"command":"echo old"}',
      },
    ]),
    storedMessage(
      "tool",
      [
        {
          type: "tool_result",
          call_id: "bg-call",
          content: "background envelope",
          is_error: false,
          images: [],
        },
      ],
      {
        details: {
          extra: {
            kind: "background_task",
            task_id: "bg-source",
            status: "running",
            command: "echo old",
          },
        },
      },
    ),
    rows[1]!,
  ];
  const cut = { kind: "message", message_id: "a1", side: "after" } as const;
  await boot(cut, null, taskRows);
  await page.getByTitle("从这条回复后分支").click();
  await expect
    .poll(() => mounted?.router.currentRoute.value.path)
    .toBe("/conversation/branch");
  await expect.poll(chatText).toContain("原回复");
  await expect.poll(chatText).toContain("来源会话历史任务");
  await expect
    .element(page.getByTitle("停止此后台任务"))
    .not.toBeInTheDocument();
  await expect.element(page.getByTitle("查看输出文件")).not.toBeInTheDocument();
  const requests = await getRequests();
  expect(
    requests.find((request) => request.path.endsWith("/fork"))?.body,
  ).toMatchObject({ cut });
  expect(requests.some((request) => request.path.includes("bg-source"))).toBe(
    false,
  );
});

async function bootAbort(cut: ForkCut = { kind: "head" }) {
  await resetMock();
  const interrupted = [
    storedMessage("user", [{ type: "text", text: "run" }], {
      id: "abort-user",
      fork_cut: { kind: "message", message_id: "abort-user", side: "before" },
    }),
    storedMessage(
      "assistant",
      [
        {
          type: "tool_call",
          id: "abort-call",
          name: "execute_command",
          arguments: '{"command":"sleep 60"}',
        },
      ],
      { id: "abort-assistant" },
    ),
    storedMessage(
      "tool",
      [
        {
          type: "tool_result",
          call_id: "abort-call",
          content: "用户取消了执行",
          is_error: true,
          images: [],
        },
      ],
      {
        id: "abort-result",
        fork_cut: {
          kind: "message",
          message_id: "abort-result",
          side: "after",
        },
      },
    ),
  ];
  const inherited = interrupted.map((row) => ({ ...row, inherited: true }));
  await setScenario({
    projects: [projectFixture()],
    conversations: [conversationFixture("c1", "p1", "中断会话")],
    providers: [providerFixture()],
    messages: { c1: [] },
    contextRevisions: { c1: 0 },
    resumable: { "abort-branch": true },
    scripts: {
      c1: [
        {
          type: "events",
          events: [
            toolCallStart(
              "abort-call",
              "execute_command",
              "sleep 60",
              '{"command":"sleep 60"}',
            ),
          ],
        },
        { type: "hang" },
      ],
      "abort-branch": [
        { type: "events", events: [textDelta("分支恢复完成"), finishEvent()] },
      ],
    },
    afterStreamHistory: {
      c1: { messages: interrupted, revision: 3, resumable: true },
      "abort-branch": {
        messages: [
          ...inherited,
          storedMessage("assistant", [{ type: "text", text: "分支恢复完成" }]),
        ],
        revision: 4,
        resumable: false,
      },
    },
    forkReplies: {
      c1: {
        response: {
          conversation: {
            ...conversationFixture("abort-branch", "p1", "中断会话（分支）"),
            forked_from_conversation_id: "c1",
            forked_at_revision: 3,
          },
          context_revision: 3,
          cut,
          draft: null,
        },
        messages: inherited,
      },
    },
  });
  mounted = await mountApp();
  await mounted.router.push("/conversation/c1");
  await sendChatMessage("run");
  await expect.poll(chatText).toContain("sleep 60");
  await page.getByTitle("停止").click();
  await expect.element(page.getByTitle("从中断处分支")).toBeEnabled();
  await page.getByRole("button", { name: TERMINAL_CARD_HEADER }).click();
  await expect.poll(chatText).toContain("用户取消了执行");
}

it("停止工具后可从中断处分支，分支能恢复此轮且来源保持中断", async () => {
  await bootAbort();
  await page.getByTitle("从中断处分支").click();
  await expect
    .poll(() => mounted?.router.currentRoute.value.path)
    .toBe("/conversation/abort-branch");
  await expect.poll(chatText).toContain("run");
  await expect
    .element(page.getByRole("button", { name: "恢复此轮" }))
    .toBeInTheDocument();
  let requests = await getRequests();
  expect(
    requests.find((request) => request.path.endsWith("/fork"))?.body,
  ).toMatchObject({ expected_revision: 3, cut: { kind: "head" } });
  expect(
    requests.some(
      (request) =>
        request.method === "POST" && request.path.endsWith("/resume"),
    ),
  ).toBe(false);
  await page.getByRole("button", { name: "恢复此轮" }).click();
  await expect.poll(chatText).toContain("分支恢复完成");
  await expect
    .element(page.getByRole("button", { name: "恢复此轮" }))
    .not.toBeInTheDocument();
  await mounted!.router.push("/conversation/c1");
  await expect.element(page.getByTitle("从中断处分支")).toBeInTheDocument();
  await expect.poll(chatText).not.toContain("分支恢复完成");
  requests = await getRequests();
  expect(
    requests
      .filter(
        (request) =>
          request.method === "POST" && request.path.endsWith("/resume"),
      )
      .map((request) => request.path),
  ).toEqual(["/api/conversations/abort-branch/resume"]);
});

it("中断工具卡片的消息菜单传递闭合工具组的真实结果 id", async () => {
  const cut = {
    kind: "message",
    message_id: "abort-result",
    side: "after",
  } as const;
  await bootAbort(cut);
  await page.getByTitle("从这一步后分支").click();
  await expect
    .poll(() => mounted?.router.currentRoute.value.path)
    .toBe("/conversation/abort-branch");
  const requests = await getRequests();
  expect(
    requests.find((request) => request.path.endsWith("/fork"))?.body,
  ).toMatchObject({ expected_revision: 3, cut });
});
