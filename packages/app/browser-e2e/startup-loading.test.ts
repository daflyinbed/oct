import { expect, it } from "vitest";
import { page } from "vitest/browser";
import client from "@/api/client";
import { useProviders } from "@/composables/useProviders";
import { mountApp } from "./support/app";
import {
  conversationFixture,
  finishEvent,
  projectFixture,
  providerFixture,
  runMeta,
  storedMessage,
  textDelta,
} from "./support/fixtures";
import { chatText } from "./support/harness";
import { getRequests, openGate, resetMock, setScenario } from "./support/mock";
import type { Middleware } from "openapi-fetch";

it("reattaches a running conversation before the configured catalog returns and defers the full catalog until settings", async () => {
  await resetMock();
  const provider = providerFixture();
  provider.models[0]!.reasoning = true;
  provider.models[0]!.reasoning_options = [
    { type: "effort", values: ["high"] },
  ];
  await setScenario({
    projects: [projectFixture()],
    conversations: [conversationFixture()],
    providers: [provider],
    running: { c1: true },
    contextRevisions: { c1: 1 },
    messages: {
      c1: [
        storedMessage("user", [{ type: "text", text: "previous request" }], {
          id: "previous-user",
          appended_at_revision: 1,
          provider_id: provider.id,
          model_id: "model-a",
          details: { extra: { reasoning: { type: "effort", effort: "high" } } },
        }),
      ],
    },
    scripts: {
      c1: [
        {
          type: "events",
          events: [
            runMeta(1, "previous-user"),
            textDelta("reattached before models"),
          ],
        },
        { type: "gate", name: "run-end" },
        { type: "events", events: [finishEvent()] },
      ],
    },
  });
  let release!: () => void;
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  const scopes: string[] = [];
  let blocked = false;
  const slowCatalog: Middleware = {
    async onResponse({ request, response }) {
      const url = new URL(request.url);
      if (url.pathname === "/api/providers") {
        scopes.push(url.searchParams.get("configured") ?? "missing");
        if (url.searchParams.get("configured") === "true") {
          blocked = true;
          await gate;
        }
      }
      return response;
    },
  };
  client.use(slowCatalog);
  window.history.replaceState(window.history.state, "", "/conversation/c1");
  const mounted = await mountApp();
  try {
    await expect.poll(() => blocked).toBe(true);
    await expect.poll(chatText).toContain("reattached before models");
    expect(scopes).toEqual(["true"]);
    expect(
      (await getRequests()).some(
        (request) => request.path === "/api/conversations/c1/events",
      ),
    ).toBe(true);
    release();
    await useProviders().whenProvidersReady();
    await expect.element(page.getByTitle("思考档位")).toHaveTextContent("high");
    await page.getByTitle("设置", { exact: true }).click();
    await expect.poll(() => scopes).toEqual(["true", "false"]);
  } finally {
    release();
    await openGate("run-end");
    mounted.unmount();
    client.eject(slowCatalog);
  }
});
