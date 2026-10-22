import type { ReadyAttachment } from "./attachments";
import type { ApiMessage } from "./display";
import type { components } from "@/api/schema";

export type SendBody = Omit<
  components["schemas"]["SendMessageRequest"],
  "provider_spec"
> & { provider_spec?: string };

type Confirmation =
  | { kind: "pending" }
  | { kind: "uncertain" }
  | { kind: "confirmed"; proof: "response" | "receipt" };

export function createSubmission(
  conversationId: string,
  content: string,
  revision: number | null,
  providerSpec?: string | null,
  reasoning?: components["schemas"]["ReasoningRequestDto"] | null,
  attachments: ReadonlyArray<ReadyAttachment> = [],
  onAccepted?: () => void,
) {
  // getRandomValues also works on HTTP workspaces without a secure context.
  const requestId = crypto.getRandomValues(new Uint32Array(4)).join("-");
  const body: SendBody = { content, client_request_id: requestId };
  if (revision != null) body.expected_revision = revision;
  if (providerSpec) body.provider_spec = providerSpec;
  if (reasoning) body.reasoning = reasoning;
  if (attachments.length > 0)
    body.attachment_ids = attachments.map(
      (attachment) => attachment.attachmentId,
    );
  let confirmation: Confirmation = { kind: "pending" };
  function confirm(proof: "response" | "receipt") {
    if (confirmation.kind === "confirmed") return;
    confirmation = { kind: "confirmed", proof };
    onAccepted?.();
  }
  function uncertain() {
    if (confirmation.kind !== "confirmed") confirmation = { kind: "uncertain" };
  }
  function receipt(history: readonly ApiMessage[]) {
    return history.find(
      (message) =>
        message.role === "user" &&
        message.details?.extra?.client_request_id === requestId,
    );
  }
  return {
    conversationId,
    requestId,
    body,
    confirm,
    uncertain,
    receipt,
    get accepted() {
      return confirmation.kind === "confirmed";
    },
    get confirmation() {
      return confirmation;
    },
  };
}

export type Submission = ReturnType<typeof createSubmission>;
