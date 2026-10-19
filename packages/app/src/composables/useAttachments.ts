import { computed, ref } from "vue";
import type { ReadyAttachment } from "./chat/attachments";
import type { components } from "@/api/schema";

export type { ComposerDraft, ReadyAttachment } from "./chat/attachments";

type AttachmentDto = components["schemas"]["AttachmentDto"];

/** 单附件上传上限（与后端 `attachments::MAX_ATTACHMENT_BYTES` 一致）。 */
export const MAX_ATTACHMENT_BYTES = 10 * 1024 * 1024;
/** 单条消息附件数上限（与后端一致）。 */
export const MAX_ATTACHMENTS_PER_MESSAGE = 8;
/** 上传并发：本地回环通常瞬完，2 路足够填满进度又不挤占页面。 */
const UPLOAD_CONCURRENCY = 2;

export type AttachmentStatus = "queued" | "uploading" | "ready" | "failed";

/** composer 里一枚附件 chip 的状态。 */
export interface PendingAttachment {
  /** 客户端本地 id（chip 的 key），与服务端附件 id 无关。 */
  localId: string;
  file: File | null;
  /** 历史草稿的源资产；恢复/重试时签发新的 staged ticket，复用字节。 */
  sourceAttachmentId: string | null;
  filename: string;
  mimeType: string;
  sizeBytes: number;
  /** 图片预览（本地 objectURL 或历史附件地址；objectURL 在移除时 revoke）。 */
  objectUrl: string | null;
  status: AttachmentStatus;
  /** 0-100，XHR upload.onprogress。 */
  progress: number;
  error: string | null;
  /** 服务端附件 id，ready 后才有。 */
  attachmentId: string | null;
}

/**
 * 聊天附件上传队列：addFiles 入队即传（ZCode 的「选完即传，发送只带
 * ready 引用」），XHR 承载上传进度；发送门禁 = 全部 ready。
 * 状态不跨会话——conversationId 变化时由 ChatPanel 调 reset()。
 */
export function useAttachments(conversationId: () => string | null) {
  const items = ref<PendingAttachment[]>([]);
  /** 拒收原因（超限/类型），chip 行上方瞬时展示。 */
  const notice = ref<string | null>(null);
  let noticeTimer: ReturnType<typeof setTimeout> | undefined;

  const allReady = computed(() =>
    items.value.every((it) => it.status === "ready"),
  );
  const uploading = computed(() =>
    items.value.some(
      (it) => it.status === "uploading" || it.status === "queued",
    ),
  );

  function showNotice(text: string) {
    notice.value = text;
    clearTimeout(noticeTimer);
    noticeTimer = setTimeout(() => (notice.value = null), 4000);
  }

  function revoke(item: PendingAttachment) {
    if (item.objectUrl?.startsWith("blob:"))
      URL.revokeObjectURL(item.objectUrl);
    item.objectUrl = null;
  }

  /**
   * 拖拽/粘贴进来的文件入队：数量与大小前置校验（零网络成本），类型
   * 交给服务端权威裁决（失败落在 chip 的 error 上）。
   */
  function addFiles(files: Iterable<File>): void {
    for (const file of files) {
      if (items.value.length >= MAX_ATTACHMENTS_PER_MESSAGE) {
        showNotice(`单条消息最多 ${MAX_ATTACHMENTS_PER_MESSAGE} 个附件`);
        return;
      }
      if (file.size > MAX_ATTACHMENT_BYTES) {
        showNotice(
          `「${file.name}」超过 ${MAX_ATTACHMENT_BYTES / 1024 / 1024} MiB 上限`,
        );
        continue;
      }
      items.value.push({
        // HTTP 工作区也支持 getRandomValues；randomUUID 要求安全上下文。
        localId: crypto.getRandomValues(new Uint32Array(4)).join("-"),
        file,
        sourceAttachmentId: null,
        filename: file.name || "file",
        mimeType: file.type || "application/octet-stream",
        sizeBytes: file.size,
        objectUrl: file.type.startsWith("image/")
          ? URL.createObjectURL(file)
          : null,
        status: "queued",
        progress: 0,
        error: null,
        attachmentId: null,
      });
      void pump();
    }
  }

  /** fork/revert 草稿保留原文件，复制入目标会话后才允许发送。 */
  function restoreDraft(attachments: readonly AttachmentDto[]): void {
    reset();
    items.value = attachments.map((attachment) => ({
      localId: crypto.getRandomValues(new Uint32Array(4)).join("-"),
      file: null,
      sourceAttachmentId: attachment.id,
      filename: attachment.filename,
      mimeType: attachment.mime_type,
      sizeBytes: attachment.size_bytes,
      objectUrl: attachment.mime_type.toLowerCase().startsWith("image/")
        ? `/api/attachments/${attachment.id}`
        : null,
      status: "queued",
      progress: 0,
      error: null,
      attachmentId: null,
    }));
    pump();
  }

  /** 每个在传请求拥有独立槽位，移除/切会话同步取消并释放。 */
  const active = new Map<string, AbortController>();
  function pump() {
    while (active.size < UPLOAD_CONCURRENCY) {
      const next = items.value.find((it) => it.status === "queued");
      if (!next) return;
      const controller = new AbortController();
      active.set(next.localId, controller);
      next.status = "uploading";
      void uploadOne(next, controller.signal).finally(() => {
        // 已取消的旧请求不能释放或推进新队列的槽位。
        if (active.get(next.localId) !== controller) return;
        active.delete(next.localId);
        pump();
      });
    }
  }

  function cancelUpload(localId: string) {
    const controller = active.get(localId);
    if (!controller) return;
    active.delete(localId);
    controller.abort();
  }

  async function uploadOne(item: PendingAttachment, signal: AbortSignal) {
    const cid = conversationId();
    if (!cid) {
      item.status = "failed";
      item.error = "尚未选择会话";
      return;
    }
    try {
      const dto = item.file
        ? await uploadXhr(
            cid,
            item.file,
            (fraction) => {
              if (!signal.aborted) item.progress = Math.round(fraction * 100);
            },
            signal,
          )
        : await copyAttachment(cid, item.sourceAttachmentId!, signal);
      if (signal.aborted) return;
      item.attachmentId = dto.id;
      item.filename = dto.filename;
      item.mimeType = dto.mime_type;
      item.sizeBytes = dto.size_bytes;
      if (item.sourceAttachmentId && item.objectUrl)
        item.objectUrl = `/api/attachments/${dto.id}`;
      item.status = "ready";
      item.progress = 100;
      // 上传完成后字节已在服务端，本地预览不再需要 File 本体之外的东西；
      // objectURL 留给 chip 缩略图直到移除/清空。
    } catch (error) {
      if (signal.aborted) return;
      item.status = "failed";
      item.error = error instanceof Error ? error.message : "上传失败";
    }
  }

  async function copyAttachment(
    cid: string,
    sourceAttachmentId: string,
    signal: AbortSignal,
  ): Promise<AttachmentDto> {
    const response = await fetch(
      `/api/conversations/${cid}/attachments/${sourceAttachmentId}/copy`,
      { method: "POST", signal },
    );
    if (response.status !== 201) {
      let message = `恢复附件失败（HTTP ${response.status}）`;
      try {
        const body = (await response.json()) as { error?: string };
        if (body.error) message = body.error;
      } catch {
        // 保留默认文案，失败 chip 可重试同一个源附件。
      }
      throw new Error(message);
    }
    return (await response.json()) as AttachmentDto;
  }

  /** XHR 才有 upload.onprogress（fetch 没有）；失败时提取后端错误文案。 */
  function uploadXhr(
    conversationId: string,
    file: File,
    onProgress: (fraction: number) => void,
    signal: AbortSignal,
  ): Promise<AttachmentDto> {
    return new Promise((resolve, reject) => {
      if (signal.aborted) {
        reject(new Error("上传已取消"));
        return;
      }
      const xhr = new XMLHttpRequest();
      function fail(error: unknown) {
        signal.removeEventListener("abort", abort);
        reject(error);
      }
      function abort() {
        xhr.abort();
        fail(new Error("上传已取消"));
      }
      xhr.open("POST", `/api/conversations/${conversationId}/attachments`);
      xhr.addEventListener("load", () => {
        if (xhr.status === 201) {
          try {
            signal.removeEventListener("abort", abort);
            resolve(JSON.parse(xhr.responseText) as AttachmentDto);
          } catch {
            fail(new Error("响应解析失败"));
          }
          return;
        }
        let message = `上传失败（HTTP ${xhr.status}）`;
        try {
          const body = JSON.parse(xhr.responseText) as { error?: string };
          if (body.error) message = body.error;
        } catch {
          // 保留默认文案
        }
        fail(new Error(message));
      });
      xhr.addEventListener("error", () => fail(new Error("网络错误")));
      xhr.addEventListener("abort", () => fail(new Error("上传已取消")));
      xhr.upload.addEventListener("progress", (event) => {
        if (event.lengthComputable) onProgress(event.loaded / event.total);
      });
      const form = new FormData();
      form.append("file", file);
      signal.addEventListener("abort", abort, { once: true });
      try {
        xhr.send(form);
      } catch (error) {
        fail(error);
      }
    });
  }

  function remove(localId: string) {
    const index = items.value.findIndex((it) => it.localId === localId);
    if (index === -1) return;
    const [item] = items.value.splice(index, 1);
    cancelUpload(localId);
    revoke(item);
    pump();
  }

  /** 失败 chip 的重试：保留 File 或源附件 id，重新入队即可。 */
  function retry(localId: string) {
    const item = items.value.find((it) => it.localId === localId);
    if (!item || item.status !== "failed") return;
    item.status = "queued";
    item.error = null;
    item.progress = 0;
    void pump();
  }

  /** 发送确认后清空本次 ready 载荷；后加的附件/failed chip 留在队列。 */
  function clearReady(payload?: ReadonlyArray<ReadyAttachment>): void {
    const sentIds = payload && new Set(payload.map((att) => att.attachmentId));
    items.value = items.value.filter((item) => {
      if (
        item.status === "ready" &&
        (!sentIds || (item.attachmentId && sentIds.has(item.attachmentId)))
      ) {
        revoke(item);
        return false;
      }
      return true;
    });
  }

  /** ready 附件的发送载荷（useChat.sendMessage 的第 5 参）。 */
  function readyPayload(): ReadyAttachment[] {
    return items.value
      .filter((it) => it.status === "ready" && it.attachmentId)
      .map((it) => ({
        attachmentId: it.attachmentId!,
        filename: it.filename,
        mimeType: it.mimeType,
      }));
  }

  /** 切换会话/组件卸载：全部丢弃并释放本地预览。 */
  function reset(): void {
    const previous = items.value;
    items.value = [];
    for (const item of previous) {
      cancelUpload(item.localId);
      revoke(item);
    }
    clearTimeout(noticeTimer);
    noticeTimer = undefined;
    notice.value = null;
  }

  return {
    items,
    notice,
    allReady,
    uploading,
    addFiles,
    restoreDraft,
    remove,
    retry,
    clearReady,
    readyPayload,
    reset,
  };
}
