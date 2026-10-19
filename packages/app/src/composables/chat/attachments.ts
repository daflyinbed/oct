import type { components } from "@/api/schema";

type AttachmentDto = components["schemas"]["AttachmentDto"];

export interface ComposerDraft {
  conversationId: string;
  text: string;
  attachments?: readonly AttachmentDto[];
}

/** 已完成上传的发送载荷；MIME 由服务端确认，供乐观消息选择展示形态。 */
export interface ReadyAttachment {
  attachmentId: string;
  filename: string;
  mimeType: string;
}
