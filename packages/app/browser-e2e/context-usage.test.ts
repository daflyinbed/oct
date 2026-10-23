import { expect, it } from "vitest";
import { page } from "vitest/browser";
import { mountApp } from "./support/app";
import {
  conversationFixture,
  finishEvent,
  projectFixture,
  providerFixture,
  storedMessage,
  textDelta,
  usageEvent,
} from "./support/fixtures";
import { sendChatMessage } from "./support/harness";
import { resetMock, setScenario } from "./support/mock";

it("restores the model context window from history and follows model changes without stale usage limits", async () => {
  const provider = providerFixture();
  provider.models[0]!.limit_context = 32_000;
  provider.models.push(
    {
      ...provider.models[0]!,
      model_id: "model-b",
      name: "Model B",
      limit_context: 200_000,
    },
    {
      ...provider.models[0]!,
      model_id: "unknown-window",
      name: "Unknown Window",
      limit_context: null,
    },
  );
  const otherProvider = providerFixture("other-provider");
  otherProvider.name = "Other Provider";
  otherProvider.models = [{ ...provider.models[1]!, limit_context: 400_000 }];
  await resetMock();
  await setScenario({
    projects: [projectFixture()],
    conversations: [{ ...conversationFixture(), permission_mode: "default" }],
    providers: [provider, otherProvider],
    messages: {
      c1: [
        storedMessage("user", [{ type: "text", text: "previous request" }], {
          provider_id: provider.id,
          model_id: "model-b",
        }),
        storedMessage("assistant", [{ type: "text", text: "previous reply" }], {
          input_tokens: 5900,
        }),
      ],
    },
    scripts: {
      c1: [
        {
          type: "events",
          events: [
            textDelta("done"),
            usageEvent(10_000, 8, 128_000),
            finishEvent(),
          ],
        },
      ],
    },
  });
  window.history.replaceState(window.history.state, "", "/conversation/c1");
  const mounted = await mountApp();
  try {
    const ring = page.getByTitle("上下文容量");
    await expect
      .element(page.getByTitle("Model", { exact: true }))
      .toHaveTextContent("Model B");
    await ring.hover();
    await expect
      .element(page.getByText("200k tokens", { exact: true }))
      .toBeInTheDocument();
    await expect
      .element(page.getByText("5.9k / 200k（3%）"))
      .toBeInTheDocument();
    await expect.element(page.getByText("85%（170k）")).toBeInTheDocument();

    await sendChatMessage("continue");
    await expect.element(page.getByTitle("发送（Enter）")).toBeInTheDocument();
    await ring.hover();
    await expect
      .element(page.getByText("128k tokens", { exact: true }))
      .toBeInTheDocument();

    await page.getByTitle("Model", { exact: true }).click();
    await page
      .getByRole("option", { name: "Model model-a", exact: true })
      .click();
    await ring.hover();
    await expect
      .element(page.getByText("32k tokens", { exact: true }))
      .toBeInTheDocument();

    await page.getByTitle("Model", { exact: true }).click();
    await page
      .getByRole("option", { name: "Unknown Window", exact: true })
      .click();
    await ring.hover();
    await expect
      .element(page.getByText("未知", { exact: true }))
      .toBeInTheDocument();

    await page.getByTitle("Provider", { exact: true }).click();
    await page
      .getByRole("option", { name: "Other Provider", exact: true })
      .click();
    await ring.hover();
    await expect
      .element(page.getByText("400k tokens", { exact: true }))
      .toBeInTheDocument();
  } finally {
    mounted.unmount();
  }
});
