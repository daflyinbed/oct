import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  MAX_ATTACHMENT_BYTES,
  MAX_ATTACHMENTS_PER_MESSAGE,
  useAttachments,
} from "./useAttachments";

// --- node 环境缺的浏览器 API ---

const objectUrlSpy = vi.fn(() => "blob:fake");
const revokeSpy = vi.fn();
beforeEach(() => {
  Object.assign(URL, {
    createObjectURL: objectUrlSpy,
    revokeObjectURL: revokeSpy,
  });
});
afterEach(() => {
  vi.restoreAllMocks();
  delete (URL as { createObjectURL?: unknown }).createObjectURL;
  delete (URL as { revokeObjectURL?: unknown }).revokeObjectURL;
});

// --- XHR shim：按文件名给剧本响应（微任务后回调，模拟异步传输） ---

interface XhrScript {
  status: number;
  body: string;
  hold?: boolean;
}
let scripts: Record<string, XhrScript> = {};
const requests: FakeXhr[] = [];

class FakeXhr {
  status = 0;
  responseText = "";
  url = "";
  aborted = false;
  private listeners = new Map<string, ((e: never) => void)[]>();
  upload = {
    addEventListener: (type: string, fn: (e: ProgressEvent) => void) =>
      this.addEventListener(type, fn as never),
  };

  addEventListener(type: string, fn: (e: never) => void) {
    const list = this.listeners.get(type) ?? [];
    list.push(fn);
    this.listeners.set(type, list);
  }

  private emit(type: string, event?: unknown) {
    for (const fn of this.listeners.get(type) ?? []) {
      (fn as (e: unknown) => void)(event);
    }
  }

  open(_method: string, url: string) {
    this.url = url;
  }
  send(form: FormData) {
    requests.push(this);
    const file = form.get("file");
    const name = file instanceof File ? file.name : "";
    const script = scripts[name] ?? { status: 500, body: "" };
    queueMicrotask(() => {
      if (!this.aborted && !script.hold) this.respond(script);
    });
  }
  respond(script: XhrScript) {
    this.emit("progress", {
      lengthComputable: true,
      loaded: 2,
      total: 4,
    } as ProgressEvent);
    this.status = script.status;
    this.responseText = script.body;
    this.emit("load");
  }
  abort() {
    this.aborted = true;
    this.emit("abort");
  }
}

function pngFile(name = "a.png", size = 4): File {
  const bytes = new Uint8Array(Math.max(size, 1));
  return new File([bytes], name, { type: "image/png" });
}

const OK_BODY = JSON.stringify({
  id: "att-1",
  filename: "a.png",
  mime_type: "image/png",
  size_bytes: 4,
});

async function ready(att: ReturnType<typeof useAttachments>) {
  await vi.waitFor(() => {
    expect(att.items.value.every((it) => it.status === "ready")).toBe(true);
  });
}

describe("useAttachments", () => {
  beforeEach(() => {
    scripts = {};
    requests.length = 0;
    vi.stubGlobal("XMLHttpRequest", FakeXhr);
  });
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("allows text-only sends with an empty queue", () => {
    const att = useAttachments(() => "conv-1");
    expect(att.allReady.value).toBe(true);
  });

  it("uploads immediately and exposes a ready payload", async () => {
    scripts["a.png"] = { status: 201, body: OK_BODY };
    const att = useAttachments(() => "conv-1");
    att.addFiles([pngFile()]);
    // addFiles 同步启动上传泵：返回时已在传。
    expect(att.items.value[0]?.status).toBe("uploading");

    await ready(att);
    expect(att.allReady.value).toBe(true);
    expect(att.items.value[0]?.attachmentId).toBe("att-1");
    expect(att.items.value[0]?.progress).toBe(100);
    expect(att.readyPayload()).toEqual([
      { attachmentId: "att-1", filename: "a.png", mimeType: "image/png" },
    ]);
    att.clearReady();
    expect(att.allReady.value).toBe(true);
    expect(att.readyPayload()).toEqual([]);
  });

  it("uses the server's metadata for text attachment payloads", async () => {
    scripts["notes.txt"] = {
      status: 201,
      body: JSON.stringify({
        id: "text-1",
        filename: "notes.txt",
        mime_type: "text/plain",
        size_bytes: 5,
      }),
    };
    const att = useAttachments(() => "conv-1");
    att.addFiles([new File(["hello"], "notes.txt")]);
    expect(att.allReady.value).toBe(false);
    await ready(att);
    expect(att.readyPayload()).toEqual([
      { attachmentId: "text-1", filename: "notes.txt", mimeType: "text/plain" },
    ]);
  });

  it("restores image and text drafts as new staged copies in their original order", async () => {
    const source = [
      {
        id: "image-source",
        filename: "shot.png",
        mime_type: "image/png",
        size_bytes: 4,
      },
      {
        id: "text-source",
        filename: "notes.txt",
        mime_type: "text/plain",
        size_bytes: 5,
      },
    ];
    const copy = vi.fn<typeof fetch>(async (url) => {
      const attachment = source.find((item) => String(url).includes(item.id))!;
      return Response.json(
        { ...attachment, id: `${attachment.id}-copy` },
        { status: 201 },
      );
    });
    vi.stubGlobal("fetch", copy);
    const att = useAttachments(() => "branch");
    att.restoreDraft(source);
    expect(att.allReady.value).toBe(false);
    expect(att.items.value[0]?.objectUrl).toBe("/api/attachments/image-source");
    await ready(att);
    expect(att.readyPayload()).toEqual([
      {
        attachmentId: "image-source-copy",
        filename: "shot.png",
        mimeType: "image/png",
      },
      {
        attachmentId: "text-source-copy",
        filename: "notes.txt",
        mimeType: "text/plain",
      },
    ]);
    expect(copy.mock.calls.map(([url]) => url)).toEqual([
      "/api/conversations/branch/attachments/image-source/copy",
      "/api/conversations/branch/attachments/text-source/copy",
    ]);
    expect(att.items.value[0]?.objectUrl).toBe(
      "/api/attachments/image-source-copy",
    );
    expect(requests).toHaveLength(0);
    att.reset();
    expect(revokeSpy).not.toHaveBeenCalled();
  });

  it("retries a failed draft copy from the same source without a local File", async () => {
    const copy = vi
      .fn<typeof fetch>()
      .mockResolvedValueOnce(
        Response.json({ error: "restore failed" }, { status: 500 }),
      )
      .mockResolvedValueOnce(
        Response.json(JSON.parse(OK_BODY), { status: 201 }),
      );
    vi.stubGlobal("fetch", copy);
    const att = useAttachments(() => "reverted");
    att.restoreDraft([
      {
        id: "source",
        filename: "shot.png",
        mime_type: "image/png",
        size_bytes: 4,
      },
    ]);
    await vi.waitFor(() => expect(att.items.value[0]?.status).toBe("failed"));
    expect(att.items.value[0]?.error).toBe("restore failed");
    expect(att.allReady.value).toBe(false);
    att.retry(att.items.value[0]!.localId);
    await ready(att);
    expect(copy.mock.calls.map(([url]) => url)).toEqual([
      "/api/conversations/reverted/attachments/source/copy",
      "/api/conversations/reverted/attachments/source/copy",
    ]);
    expect(att.readyPayload()[0]?.attachmentId).toBe("att-1");
    att.reset();
  });

  it("aborts old draft copies on reset and ignores late responses", async () => {
    let finishOld!: (response: Response) => void;
    const copy = vi
      .fn<typeof fetch>()
      .mockImplementationOnce(
        () =>
          new Promise<Response>((resolve) => {
            finishOld = resolve;
          }),
      )
      .mockResolvedValue(Response.json(JSON.parse(OK_BODY), { status: 201 }));
    vi.stubGlobal("fetch", copy);
    let cid = "old";
    const att = useAttachments(() => cid);
    att.restoreDraft([
      {
        id: "source",
        filename: "shot.png",
        mime_type: "image/png",
        size_bytes: 4,
      },
    ]);
    const oldSignal = copy.mock.calls[0]![1]!.signal;
    cid = "new";
    att.restoreDraft([
      {
        id: "new-source",
        filename: "new.png",
        mime_type: "image/png",
        size_bytes: 4,
      },
    ]);
    expect(oldSignal?.aborted).toBe(true);
    await ready(att);
    finishOld(
      Response.json(
        { ...JSON.parse(OK_BODY), id: "old-copy" },
        { status: 201 },
      ),
    );
    await Promise.resolve();
    await Promise.resolve();
    expect(att.readyPayload()[0]?.attachmentId).toBe("att-1");
    expect(copy.mock.calls[1]![0]).toBe(
      "/api/conversations/new/attachments/new-source/copy",
    );
    att.reset();
  });

  it("uploads on HTTP workspaces where crypto.randomUUID is unavailable", async () => {
    vi.stubGlobal("crypto", {
      getRandomValues: crypto.getRandomValues.bind(crypto),
    });
    scripts["a.png"] = { status: 201, body: OK_BODY };
    const att = useAttachments(() => "conv-http");
    att.addFiles([pngFile()]);
    await ready(att);
    expect(att.items.value[0]?.localId).toEqual(expect.any(String));
    expect(requests[0]?.url).toBe("/api/conversations/conv-http/attachments");
    att.reset();
  });

  it("rejects oversize files upfront with a notice", () => {
    const att = useAttachments(() => "conv-1");
    att.addFiles([pngFile("big.png", MAX_ATTACHMENT_BYTES + 1)]);
    expect(att.items.value).toHaveLength(0);
    expect(att.notice.value).toContain("超过");
  });

  it("caps the number of attachments per message", () => {
    const att = useAttachments(() => "conv-1");
    att.addFiles(
      Array.from({ length: MAX_ATTACHMENTS_PER_MESSAGE + 1 }, (_, i) =>
        pngFile(`f${i}.png`),
      ),
    );
    expect(att.items.value).toHaveLength(MAX_ATTACHMENTS_PER_MESSAGE);
    expect(att.notice.value).toContain("最多");
  });

  it("lands server errors on the chip and retry recovers", async () => {
    scripts["bad.pdf"] = {
      status: 400,
      body: JSON.stringify({ code: "bad_request", error: "暂不支持 PDF 文件" }),
    };
    const att = useAttachments(() => "conv-1");
    att.addFiles([pngFile("bad.pdf")]);
    await vi.waitFor(() => {
      expect(att.items.value[0]?.status).toBe("failed");
    });
    expect(att.items.value[0]?.error).toBe("暂不支持 PDF 文件");
    expect(att.allReady.value).toBe(false);

    scripts["bad.pdf"] = { status: 201, body: OK_BODY };
    att.retry(att.items.value[0]!.localId);
    await ready(att);
    expect(att.allReady.value).toBe(true);
  });

  it("clearReady keeps failed chips for retry", async () => {
    scripts["ok.png"] = { status: 201, body: OK_BODY };
    scripts["no.png"] = { status: 500, body: "" };
    const att = useAttachments(() => "conv-1");
    att.addFiles([pngFile("ok.png"), pngFile("no.png")]);
    await vi.waitFor(() => {
      expect(att.items.value.every((it) => it.status !== "uploading")).toBe(
        true,
      );
    });
    att.clearReady();
    expect(att.items.value.map((it) => it.filename)).toEqual(["no.png"]);
    expect(revokeSpy).toHaveBeenCalled();
  });

  it("accepting a send preserves attachments added after its payload was captured", async () => {
    scripts["first.png"] = { status: 201, body: OK_BODY };
    scripts["next.png"] = {
      status: 201,
      body: JSON.stringify({
        id: "att-next",
        filename: "next.png",
        mime_type: "image/png",
        size_bytes: 4,
      }),
    };
    const att = useAttachments(() => "conv-1");
    att.addFiles([pngFile("first.png")]);
    await ready(att);
    const payload = att.readyPayload();
    att.addFiles([pngFile("next.png")]);
    await ready(att);
    att.clearReady(payload);
    expect(att.readyPayload().map((item) => item.attachmentId)).toEqual([
      "att-next",
    ]);
    att.reset();
  });

  it("removing an in-flight upload cancels it and starts the next queued file", () => {
    for (const name of ["first.png", "second.png", "next.png"])
      scripts[name] = { status: 201, body: OK_BODY, hold: true };
    const att = useAttachments(() => "conv-1");
    att.addFiles([
      pngFile("first.png"),
      pngFile("second.png"),
      pngFile("next.png"),
    ]);
    expect(requests).toHaveLength(2);
    att.remove(att.items.value[0]!.localId);
    expect(requests[0]?.aborted).toBe(true);
    expect(requests).toHaveLength(3);
    expect(att.items.value.map((item) => item.status)).toEqual([
      "uploading",
      "uploading",
    ]);
    att.reset();
  });

  it("reset frees old upload slots and ignores their late responses", async () => {
    for (const name of [
      "old-a.png",
      "old-b.png",
      "old-queued.png",
      "new-a.png",
      "new-b.png",
      "new-queued.png",
    ])
      scripts[name] = { status: 201, body: OK_BODY, hold: true };
    let conversationId = "old";
    const att = useAttachments(() => conversationId);
    att.addFiles([
      pngFile("old-a.png"),
      pngFile("old-b.png"),
      pngFile("old-queued.png"),
    ]);
    const old = requests.slice();
    conversationId = "new";
    att.reset();
    expect(old.every((request) => request.aborted)).toBe(true);
    att.addFiles([
      pngFile("new-a.png"),
      pngFile("new-b.png"),
      pngFile("new-queued.png"),
    ]);
    expect(requests).toHaveLength(4);
    for (const request of old) request.respond({ status: 201, body: OK_BODY });
    await Promise.resolve();
    await Promise.resolve();
    expect(requests).toHaveLength(4);
    expect(att.items.value.map((item) => item.status)).toEqual([
      "uploading",
      "uploading",
      "queued",
    ]);
    requests[2]!.respond({ status: 201, body: OK_BODY });
    await vi.waitFor(() => expect(requests).toHaveLength(5));
    expect(
      requests
        .slice(2)
        .every(
          (request) => request.url === "/api/conversations/new/attachments",
        ),
    ).toBe(true);
    expect(att.items.value.map((item) => item.status)).toEqual([
      "ready",
      "uploading",
      "uploading",
    ]);
    att.reset();
  });

  it("remove drops the chip and releases its preview URL", () => {
    const att = useAttachments(() => "conv-1");
    att.addFiles([pngFile()]);
    const localId = att.items.value[0]!.localId;
    att.remove(localId);
    expect(att.items.value).toHaveLength(0);
    expect(att.allReady.value).toBe(true);
    expect(revokeSpy).toHaveBeenCalledWith("blob:fake");
  });

  it("reset clears everything", async () => {
    scripts["a.png"] = { status: 201, body: OK_BODY };
    const att = useAttachments(() => "conv-1");
    att.addFiles([pngFile()]);
    await ready(att);
    att.reset();
    expect(att.items.value).toHaveLength(0);
    expect(att.allReady.value).toBe(true);
    expect(att.readyPayload()).toEqual([]);
  });
});
