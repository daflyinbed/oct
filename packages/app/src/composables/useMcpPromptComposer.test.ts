import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { effectScope, ref } from "vue";
import { useMcpPromptComposer } from "./useMcpPromptComposer";
import { renderMcpPrompt } from "./useMcpServers";

vi.mock("./useMcpServers", () => ({ renderMcpPrompt: vi.fn() }));
const renderPrompt = vi.mocked(renderMcpPrompt);
let scope = effectScope();
function composer() {
  const conversation = ref<string | null>("a");
  const input = ref("/status extra context");
  const session = scope.run(() => useMcpPromptComposer(conversation, input))!;
  return { conversation, input, ...session };
}
function pendingRender() {
  let resolve!: (value: Awaited<ReturnType<typeof renderMcpPrompt>>) => void;
  let reject!: (cause: Error) => void;
  renderPrompt.mockImplementationOnce(
    () =>
      new Promise((done, fail) => {
        resolve = done;
        reject = fail;
      }),
  );
  return {
    complete: (text: string) => resolve({ messages: [{ role: "user", text }] }),
    reject: (cause: Error) => reject(cause),
  };
}
beforeEach(() => {
  renderPrompt.mockReset();
  scope = effectScope();
});
afterEach(() => scope.stop());

it("renders into the owned composer while preserving command suffix", async () => {
  const session = composer();
  renderPrompt.mockResolvedValue({
    messages: [{ role: "user", text: "Report status" }],
  });
  expect(await session.render("m1", "status", {})).toBe("Report status");
  expect(session.input.value).toBe("Report status extra context");
  expect(session.rendering.value).toBe(false);
});

it.each(["conversation", "input"] as const)(
  "a pending reply cannot overwrite changed %s",
  async (change) => {
    const session = composer();
    const response = pendingRender();
    const rendering = session.render("m1", "status", {});
    if (change === "conversation") session.conversation.value = "b";
    session.input.value = "new draft";
    response.complete("old prompt");
    expect(await rendering).toBeNull();
    expect(session.input.value).toBe("new draft");
    expect(session.rendering.value).toBe(false);
    expect(session.error.value).toBeNull();
  },
);

it("an older response cannot replace a newer prompt or end its loading state", async () => {
  const session = composer();
  const old = pendingRender();
  const first = session.render("m1", "status", {});
  const current = pendingRender();
  const second = session.render("m1", "deploy", {});
  old.complete("old prompt");
  expect(await first).toBeNull();
  expect(session.rendering.value).toBe(true);
  current.complete("current prompt");
  await second;
  expect(session.input.value).toBe("current prompt extra context");
  expect(session.rendering.value).toBe(false);
});

it("late errors after navigation or disposal cannot change composer state", async () => {
  const session = composer();
  const response = pendingRender();
  const rendering = session.render("m1", "status", {});
  session.conversation.value = "b";
  scope.stop();
  response.reject(new Error("old failure"));
  expect(await rendering).toBeNull();
  expect(session.error.value).toBeNull();
  expect(session.rendering.value).toBe(false);
});

it("current errors remain visible and another render can recover", async () => {
  const session = composer();
  renderPrompt.mockRejectedValueOnce(new Error("retry prompt"));
  await session.render("m1", "status", {});
  expect(session.error.value).toBe("retry prompt");
  renderPrompt.mockResolvedValueOnce({
    messages: [{ role: "user", text: "ready" }],
  });
  await session.render("m1", "status", {});
  expect(session.error.value).toBeNull();
  expect(session.input.value).toBe("ready extra context");
});
