import { describe, expect, it } from "vitest";

import { storedToDisplayList } from "./display";
import type { ApiMessage } from "./display";

function userRow(overrides: Partial<ApiMessage>): ApiMessage {
  return {
    id: "m1",
    role: "user",
    parts: [{ type: "text", text: "看这张图" }],
    ordering: 1,
    details: null,
    fork_cut: null,
    inherited: false,
    created_at: "2026-10-15T00:00:00",
    ...overrides,
  } as ApiMessage;
}

describe("storedToDisplayList user image attachments", () => {
  it("renders the explicit human body and assets without model-facing snapshot text", () => {
    const [message] = storedToDisplayList([
      userRow({
        parts: [
          {
            type: "text",
            text: "[attachment: notes.txt]\nmodel-only snapshot",
          },
        ],
        user_content: {
          body: "只显示这段正文",
          attachments: [
            {
              id: "text-1",
              filename: "notes.txt",
              mime_type: "text/plain",
              size_bytes: 5,
            },
            {
              id: "image-1",
              filename: "shot.png",
              mime_type: "image/png",
              size_bytes: 68,
            },
          ],
        },
      }),
    ]);
    expect(message?.parts).toEqual([
      { kind: "text", text: "只显示这段正文" },
      { kind: "file", attachmentId: "text-1", filename: "notes.txt" },
      { kind: "image", attachmentId: "image-1", filename: "shot.png" },
    ]);
  });

  it("builds image parts from details.extra.attachments metadata", () => {
    const rows = [
      userRow({
        parts: [
          { type: "text", text: "看这张图" },
          { type: "image_url", url: "oct-attachment://att-1" },
        ],
        details: {
          kind: null,
          compaction: null,
          reasoning_duration_ms: null,
          extra: {
            attachments: [
              { id: "att-1", filename: "shot.png", mime_type: "image/png" },
            ],
          },
        },
      }),
    ];
    const [message] = storedToDisplayList(rows);
    expect(message?.parts).toEqual([
      { kind: "text", text: "看这张图" },
      { kind: "image", attachmentId: "att-1", filename: "shot.png" },
    ]);
  });

  it("falls back to parsing oct-attachment refs when details are missing", () => {
    const rows = [
      userRow({
        parts: [
          { type: "image_url", url: "oct-attachment://att-9" },
          { type: "text", text: "hi" },
        ],
      }),
    ];
    const [message] = storedToDisplayList(rows);
    expect(message?.parts).toEqual([
      { kind: "text", text: "hi" },
      { kind: "image", attachmentId: "att-9", filename: null },
    ]);
  });

  it("renders text attachments as file links alongside image thumbnails", () => {
    const [message] = storedToDisplayList([
      userRow({
        parts: [
          { type: "text", text: "[attachment: notes.txt]\nhello" },
          { type: "image_url", url: "oct-attachment://image-1" },
        ],
        details: {
          extra: {
            attachments: [
              { id: "text-1", filename: "notes.txt", mime_type: "text/plain" },
              { id: "image-1", filename: "shot.png", mime_type: "image/png" },
            ],
          },
        },
      }),
    ]);
    expect(message?.parts).toEqual([
      { kind: "text", text: "[attachment: notes.txt]\nhello" },
      { kind: "file", attachmentId: "text-1", filename: "notes.txt" },
      { kind: "image", attachmentId: "image-1", filename: "shot.png" },
    ]);
  });

  it("recovers image references when metadata is partial or missing MIME", () => {
    const [message] = storedToDisplayList([
      userRow({
        parts: [
          { type: "image_url", url: "oct-attachment://image-1" },
          { type: "image_url", url: "oct-attachment://image-2" },
        ],
        details: {
          extra: {
            attachments: [{ id: "image-1", filename: "old.png" }, null],
          },
        },
      }),
    ]);
    expect(message?.parts).toEqual([
      { kind: "image", attachmentId: "image-1", filename: "old.png" },
      { kind: "image", attachmentId: "image-2", filename: null },
    ]);
  });

  it("leaves non-attachment image urls alone", () => {
    const rows = [
      userRow({
        parts: [{ type: "image_url", url: "data:image/png;base64,AA" }],
      }),
    ];
    const [message] = storedToDisplayList(rows);
    expect(message?.parts).toHaveLength(0);
  });
});
