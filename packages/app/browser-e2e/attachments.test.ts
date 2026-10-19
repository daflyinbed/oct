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
  bootConversation,
  chatText,
  COMPOSER_SELECTOR,
  sendChatMessage,
} from "./support/harness";
import { getRequests, openGate, resetMock, setScenario } from "./support/mock";
import type { MountedApp } from "./support/app";
import type { Scenario, ScriptStep } from "./support/mock";
import type { components } from "@/api/schema";

let mounted: MountedApp | null = null;
let nextConversation = 0;
afterEach(async () => {
  mounted?.unmount();
  mounted = null;
  await resetMock();
});

const imageAttachment: components["schemas"]["AttachmentDto"] = {
  id: "image-1",
  filename: "shot.png",
  mime_type: "image/png",
  size_bytes: 68,
};
type UploadReply = NonNullable<Scenario["attachmentReplies"]>[string][number];

async function bootAttachments(
  replies: UploadReply[],
  script: ScriptStep[] = [],
) {
  await resetMock();
  const id = `attachments-${++nextConversation}`;
  const scenario: Scenario = {
    projects: [projectFixture()],
    conversations: [conversationFixture(id)],
    providers: [providerFixture()],
    messages: { [id]: [] },
    attachmentReplies: { [id]: replies },
    scripts: { [id]: script },
  };
  await setScenario(scenario);
  mounted = await mountApp();
  await mounted.router.push(`/conversation/${id}`);
  await expect.element(page.getByPlaceholder(COMPOSER_SELECTOR)).toBeEnabled();
  return { id, scenario };
}

function pngFile(filename = "shot.png") {
  const bytes = Uint8Array.from(
    atob(
      "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/l9sAAAAASUVORK5CYII=",
    ),
    (char) => char.charCodeAt(0),
  );
  return new File([bytes], filename, { type: "image/png" });
}

async function dropFile(file: File) {
  const transfer = new DataTransfer();
  transfer.items.add(file);
  const main = await page.getByRole("main").element();
  main.dispatchEvent(
    new DragEvent("drop", {
      bubbles: true,
      cancelable: true,
      dataTransfer: transfer,
    }),
  );
}

it("shows the human body and file links without exposing model snapshots", async () => {
  mounted = await bootConversation([], {
    stored: [
      storedMessage(
        "user",
        [{ type: "text", text: "[attachment: notes.txt]\nhello" }],
        {
          user_content: {
            body: "请阅读附件",
            attachments: [
              {
                id: "text-1",
                filename: "notes.txt",
                mime_type: "text/plain",
                size_bytes: 5,
              },
            ],
          },
          details: {
            extra: {
              attachments: [
                {
                  id: "text-1",
                  filename: "notes.txt",
                  mime_type: "text/plain",
                },
              ],
            },
          },
        },
      ),
    ],
  });
  await expect.poll(chatText).toContain("请阅读附件");
  await expect.poll(chatText).not.toContain("[attachment: notes.txt]");
  await expect
    .element(page.getByRole("link", { name: "notes.txt" }))
    .toHaveAttribute("href", "/api/attachments/text-1");
  await expect
    .element(page.getByRole("img", { name: "notes.txt" }))
    .not.toBeInTheDocument();
});

it("shows an oversized first attachment's rejection even with an empty queue", async () => {
  const { id } = await bootAttachments([]);
  await dropFile(
    new File([new Uint8Array(10 * 1024 * 1024 + 1)], "too-large.txt"),
  );
  await expect.poll(chatText).toContain("「too-large.txt」超过 10 MiB 上限");
  const requests = await getRequests();
  expect(
    requests.filter(
      (request) => request.path === `/api/conversations/${id}/attachments`,
    ),
  ).toHaveLength(0);
});

it("shows image upload errors and retries the original file", async () => {
  const { id } = await bootAttachments([
    { status: 500, body: { error: "上传暂时失败，请重试" } },
    { status: 201, body: imageAttachment },
  ]);
  await dropFile(pngFile());
  await expect
    .element(page.getByRole("alert"))
    .toHaveTextContent("上传暂时失败，请重试");
  await expect.element(page.getByTitle("等待附件上传完成")).toBeDisabled();
  await page.getByTitle("重试").click();
  await expect.element(page.getByTitle("发送（Enter）")).toBeEnabled();
  await expect.element(page.getByRole("alert")).not.toBeInTheDocument();
  const requests = await getRequests();
  expect(
    requests.filter(
      (request) => request.path === `/api/conversations/${id}/attachments`,
    ),
  ).toHaveLength(2);
});

it("retains a rejected message's draft and retries without uploading again", async () => {
  const { id, scenario } = await bootAttachments(
    [{ status: 201, body: imageAttachment }],
    [{ type: "status", code: 409 }],
  );
  await dropFile(pngFile());
  await expect.element(page.getByTitle("发送（Enter）")).toBeEnabled();
  await sendChatMessage("keep this image");
  await expect.poll(chatText).toContain("Failed to send message.");
  await expect
    .element(page.getByPlaceholder(COMPOSER_SELECTOR))
    .toHaveValue("keep this image");
  await expect.element(page.getByTitle("移除")).toBeInTheDocument();

  // 只改发送剧本；同一 ready id 应直接重试，不能另发一次上传。
  scenario.scripts = { [id]: [{ type: "events", events: [finishEvent()] }] };
  await setScenario(scenario);
  await page.getByTitle("发送（Enter）").click();
  await expect.element(page.getByPlaceholder(COMPOSER_SELECTOR)).toBeEnabled();
  await expect
    .element(page.getByPlaceholder(COMPOSER_SELECTOR))
    .toHaveValue("");
  await expect.element(page.getByTitle("移除")).not.toBeInTheDocument();
  const requests = await getRequests();
  const messages = requests.filter(
    (request) =>
      request.method === "POST" &&
      request.path === `/api/conversations/${id}/messages`,
  );
  expect(messages).toHaveLength(2);
  for (const request of messages)
    expect(
      (request.body as components["schemas"]["SendMessageRequest"])
        .attachment_ids,
    ).toEqual(["image-1"]);
  expect(
    requests.filter(
      (request) => request.path === `/api/conversations/${id}/attachments`,
    ),
  ).toHaveLength(1);
});

it("clears only the accepted payload before SSE finishes and keeps the next attachment", async () => {
  const { id } = await bootAttachments(
    [
      { status: 201, body: imageAttachment },
      {
        status: 201,
        body: { ...imageAttachment, id: "image-next", filename: "next.png" },
      },
    ],
    [
      { type: "gate", name: "accept-send" },
      { type: "events", events: [textDelta("accepted")] },
      { type: "gate", name: "finish-send" },
      { type: "events", events: [finishEvent()] },
    ],
  );
  await dropFile(pngFile());
  await expect.element(page.getByTitle("发送（Enter）")).toBeEnabled();
  await sendChatMessage("first image");
  await expect
    .poll(async () =>
      (await getRequests()).some(
        (request) =>
          request.method === "POST" &&
          request.path === `/api/conversations/${id}/messages`,
      ),
    )
    .toBe(true);
  await expect.element(page.getByTitle("移除")).toBeInTheDocument();
  await dropFile(pngFile("next.png"));
  await expect
    .poll(
      async () =>
        (await getRequests()).filter(
          (request) => request.path === `/api/conversations/${id}/attachments`,
        ).length,
    )
    .toBe(2);
  await openGate("accept-send");
  await expect.poll(chatText).toContain("accepted");
  await expect.element(page.getByTitle("停止")).toBeInTheDocument();
  await expect
    .element(page.getByPlaceholder(COMPOSER_SELECTOR))
    .toHaveValue("");
  await expect
    .poll(() => document.querySelectorAll('button[title="移除"]').length)
    .toBe(1);
  await expect
    .element(page.getByRole("img", { name: "next.png" }))
    .toBeInTheDocument();
  await openGate("finish-send");
  await expect.element(page.getByTitle("发送（Enter）")).toBeEnabled();
  await expect.element(page.getByTitle("移除")).toBeInTheDocument();
});

it.each([false, true])(
  "restores and resends a reverted attachment draft (mixed=%s)",
  async (mixed) => {
    await resetMock();
    const id = `restored-attachments-${++nextConversation}`;
    const source = conversationFixture(id);
    const sourceAttachments = [imageAttachment];
    if (mixed)
      sourceAttachments.push({
        id: "text-source",
        filename: "notes.txt",
        mime_type: "text/plain",
        size_bytes: 15,
      });
    const staged = sourceAttachments.map((attachment) => ({
      ...attachment,
      id: `${attachment.id}-copy`,
    }));
    const text = mixed ? "editable request" : "";
    const userParts: components["schemas"]["MessagePart"][] = [
      ...(mixed ? [{ type: "text" as const, text }] : []),
      { type: "image_url", url: `oct-attachment://${imageAttachment.id}` },
      ...(mixed
        ? [
            {
              type: "text" as const,
              text: "[attachment: notes.txt]\nattachment body",
            },
          ]
        : []),
    ];
    const cut = {
      kind: "message",
      message_id: "restore-user",
      side: "before",
    } as const;
    const original = [
      storedMessage("user", userParts, {
        id: "restore-user",
        fork_cut: cut,
        details: { extra: { attachments: sourceAttachments } },
      }),
      storedMessage("assistant", [{ type: "text", text: "original answer" }]),
    ];
    const restoredParts = userParts.map((part) =>
      part.type === "image_url"
        ? { ...part, url: `oct-attachment://${staged[0]!.id}` }
        : part,
    );
    const gate = `restore-copy-${id}`;
    const backupId = `backup-${id}`;
    await setScenario({
      projects: [projectFixture()],
      conversations: [source],
      providers: [providerFixture()],
      messages: { [id]: original },
      contextRevisions: { [id]: 2 },
      revertReplies: {
        [id]: {
          response: {
            conversation: { ...source, history_epoch: 1 },
            backup: {
              ...conversationFixture(backupId),
              origin_kind: "revert_backup",
              backup_of_conversation_id: id,
              backup_at_revision: 2,
            },
            backup_conversation_id: backupId,
            context_revision: 3,
            cut,
            draft: text,
            draft_attachments: sourceAttachments,
          },
          messages: [],
          backupMessages: original,
        },
      },
      attachmentReplies: {
        [id]: staged.map((attachment, index) => ({
          status: 201,
          body: attachment,
          ...(index === 0 ? { gate } : {}),
        })),
      },
      scripts: {
        [id]: [
          {
            type: "events",
            events: [textDelta("restored answer"), finishEvent()],
          },
        ],
      },
      afterStreamHistory: {
        [id]: {
          revision: 5,
          messages: [
            storedMessage("user", restoredParts, {
              details: { extra: { attachments: staged } },
            }),
            storedMessage("assistant", [
              { type: "text", text: "restored answer" },
            ]),
          ],
        },
      },
    });
    mounted = await mountApp();
    await mounted.router.push(`/conversation/${id}`);
    await expect.element(page.getByTitle("回退并编辑")).toBeEnabled();
    await page.getByTitle("回退并编辑").click();
    await page.getByRole("button", { name: "确认回退" }).click();
    await expect
      .element(page.getByPlaceholder(COMPOSER_SELECTOR))
      .toHaveValue(text);
    await expect.element(page.getByTitle("等待附件上传完成")).toBeDisabled();
    await expect
      .element(page.getByRole("img", { name: "shot.png" }))
      .toHaveAttribute("src", `/api/attachments/${imageAttachment.id}`);
    await openGate(gate);
    await expect.element(page.getByTitle("发送（Enter）")).toBeEnabled();
    await expect
      .element(page.getByRole("img", { name: "shot.png" }))
      .toHaveAttribute("src", `/api/attachments/${staged[0]!.id}`);
    await page.getByTitle("发送（Enter）").click();
    await expect.poll(chatText).toContain("restored answer");
    await expect.element(page.getByTitle("移除")).not.toBeInTheDocument();
    const requests = await getRequests();
    const send = requests.find(
      (request) =>
        request.method === "POST" &&
        request.path === `/api/conversations/${id}/messages`,
    );
    expect(send?.body).toMatchObject({
      content: text,
      attachment_ids: staged.map((attachment) => attachment.id),
    });
    expect(
      requests
        .filter((request) => request.path.endsWith("/copy"))
        .map((request) => request.path),
    ).toEqual(
      sourceAttachments.map(
        (attachment) =>
          `/api/conversations/${id}/attachments/${attachment.id}/copy`,
      ),
    );
  },
);
