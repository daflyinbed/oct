import { expect, it } from "vitest";
import { page } from "vitest/browser";
import { mountApp } from "./support/app";
import {
  conversationFixture,
  finishEvent,
  projectFixture,
  providerFixture,
  textDelta,
} from "./support/fixtures";
import {
  chatText,
  COMPOSER_SELECTOR,
  sendChatMessage,
} from "./support/harness";
import { getRequests, resetMock, setScenario } from "./support/mock";

it("selects catalog effort and returns to default without a SelectItem error", async () => {
  const provider = providerFixture();
  provider.adapter_type = "openai_compatible";
  provider.models[0]!.reasoning = true;
  provider.models[0]!.reasoning_options = [
    { type: "effort", values: ["low", "high", "max"] },
  ];
  await resetMock();
  await setScenario({
    projects: [projectFixture()],
    conversations: [conversationFixture()],
    providers: [provider],
    messages: { c1: [] },
    scripts: {
      c1: [{ type: "events", events: [textDelta("done"), finishEvent()] }],
    },
  });
  const mounted = await mountApp();
  try {
    await mounted.router.push("/conversation/c1");
    await expect
      .element(page.getByPlaceholder(COMPOSER_SELECTOR))
      .toBeInTheDocument();
    await page.getByTitle("思考档位").click();
    await page.getByRole("option", { name: "max", exact: true }).click();
    await sendChatMessage("with effort");
    await expect.poll(chatText).toContain("done");
    const first = (await getRequests()).find(
      (request) =>
        request.path === "/api/conversations/c1/messages" &&
        request.method === "POST",
    );
    expect(first?.body.reasoning).toEqual({ type: "effort", effort: "max" });

    await expect.element(page.getByTitle("发送（Enter）")).toBeInTheDocument();
    await page.getByTitle("思考档位").click();
    await page.getByRole("option", { name: "默认", exact: true }).click();
    await sendChatMessage("with default");
    await expect
      .poll(
        async () =>
          (await getRequests()).filter(
            (request) =>
              request.path === "/api/conversations/c1/messages" &&
              request.method === "POST",
          ).length,
      )
      .toBe(2);
    const requests = (await getRequests()).filter(
      (request) =>
        request.path === "/api/conversations/c1/messages" &&
        request.method === "POST",
    );
    expect(requests[1]?.body).not.toHaveProperty("reasoning");
    await expect.element(page.getByTitle("发送（Enter）")).toBeInTheDocument();
  } finally {
    mounted.unmount();
  }
});
