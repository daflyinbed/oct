import { expect, it } from "vitest";
import { readChatStream } from "./transport";

it("decodes split UTF-8, CRLF and multiline SSE data without treating EOF as success", async () => {
  const bytes = new TextEncoder().encode('data: {"type":"text_delta",\r\ndata: "data":"恢复"}\r\n\r\n');
  const seen: unknown[] = [];
  const response = new Response(new ReadableStream({
    start(controller) {
      for (const byte of bytes) controller.enqueue(new Uint8Array([byte]));
      controller.close();
    },
  }));
  expect(await readChatStream(response, event => seen.push(event))).toBe(false);
  expect(seen).toEqual([{ type: "text_delta", data: "恢复" }]);
});

it("requires a terminal event, not just the provider DONE sentinel", async () => {
  const response = new Response('data: [DONE]\n\n');
  expect(await readChatStream(response, () => {})).toBe(false);
});
