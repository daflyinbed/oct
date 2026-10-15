import { expect, it, vi } from "vitest";
import { page } from "vitest/browser";
import { nextTick } from "vue";
import client from "@/api/client";
import { useChat } from "@/composables/useChat";
import { mountApp } from "./support/app";
import {
  conversationFixture,
  finishEvent,
  projectFixture,
  providerFixture,
  storedMessage,
  textDelta,
} from "./support/fixtures";
import { sendChatMessage } from "./support/harness";
import { getRequests, openGate, resetMock, setScenario } from "./support/mock";
import type { Scenario } from "./support/mock";
import type { Middleware } from "openapi-fetch";

async function prepare(overrides: Partial<Scenario> = {}) {
  const provider = providerFixture();
  provider.adapter_type = "openai_compatible";
  provider.models[0]!.reasoning = true;
  provider.models[0]!.reasoning_options = [
    { type: "effort", values: ["low", "high"] },
  ];
  provider.models.push({
    ...provider.models[0]!,
    model_id: "model-b",
    name: "Model B",
  });
  const request = (effort: string) =>
    storedMessage("user", [{ type: "text", text: effort }], {
      provider_id: provider.id,
      model_id: "model-a",
      details: { extra: { reasoning: { type: "effort", effort } } },
    });
  const script = [
    { type: "events" as const, events: [textDelta("done"), finishEvent()] },
  ];
  await resetMock();
  await setScenario({
    projects: [projectFixture()],
    conversations: ["c1", "c2"].map((id) => ({
      ...conversationFixture(id),
      permission_mode: "default",
    })),
    providers: [provider],
    messages: { c1: [request("low")], c2: [request("high")] },
    scripts: { c1: script, c2: script },
    ...overrides,
  });
  window.history.replaceState(window.history.state, "", "/");
}

async function waitForAttachment(id: string) {
  await expect
    .poll(async () =>
      (await getRequests()).some(
        (r) => r.path === `/api/conversations/${id}/subagents`,
      ),
    )
    .toBe(true);
}

it.each([false, true])(
  "preserves a manual reasoning choice after a late history response (return to first: %s)",
  async (returnToFirst) => {
    await prepare({ historyReplies: { c1: [{ gate: "old-history" }] } });
    // 仅观察公开加载方法返回的 Promise，确保旧请求完整落定后再断言；
    // HTTP/代理/历史转换仍走真实路径，不用 sleep 等待竞态。
    const historyLoads = vi.spyOn(useChat(), "fetchMessages");
    // Chromium 默认会合并同 URL 的在飞 GET；禁用缓存使 A 的两次请求独立。
    const noCache: Middleware = {
      onRequest: ({ request }) => new Request(request, { cache: "no-store" }),
    };
    client.use(noCache);
    const mounted = await mountApp();
    try {
      await mounted.router.push("/conversation/c1");
      await expect
        .poll(async () =>
          (await getRequests()).some(
            (r) => r.path === "/api/conversations/c1/messages",
          ),
        )
        .toBe(true);
      const oldLoad = historyLoads.mock.results[0]!.value;
      await mounted.router.push("/conversation/c2");
      await waitForAttachment("c2");
      await expect
        .element(page.getByTitle("思考档位"))
        .toHaveTextContent("high");

      if (returnToFirst) {
        await mounted.router.push("/conversation/c1");
        await waitForAttachment("c1");
        await expect
          .element(page.getByTitle("思考档位"))
          .toHaveTextContent("low");
      }
      const effort = returnToFirst ? "high" : "low";
      await page.getByTitle("思考档位").click();
      await page.getByRole("option", { name: effort, exact: true }).click();
      await openGate("old-history");
      await oldLoad;
      await nextTick();
      await expect
        .element(page.getByTitle("思考档位"))
        .toHaveTextContent(effort);

      await sendChatMessage("keep my reasoning choice");
      await expect
        .poll(
          async () =>
            (await getRequests()).find(
              (r) => r.method === "POST" && r.path.endsWith("/messages"),
            )?.body,
        )
        .toMatchObject({ reasoning: { type: "effort", effort } });
      await expect
        .element(page.getByTitle("发送（Enter）"))
        .toBeInTheDocument();
    } finally {
      await openGate("old-history");
      mounted.unmount();
      historyLoads.mockRestore();
      client.eject(noCache);
    }
  },
);

it("keeps the current model when a new conversation's history fails to load", async () => {
  await prepare({
    messages: {
      c1: [
        storedMessage("user", [{ type: "text", text: "previous" }], {
          provider_id: "mock-prov",
          model_id: "model-a",
        }),
      ],
      c2: [],
    },
    historyReplies: { c2: [{ status: 500 }] },
  });
  const historyLoads = vi.spyOn(useChat(), "fetchMessages");
  const mounted = await mountApp();
  try {
    await mounted.router.push("/conversation/c1");
    await waitForAttachment("c1");
    await page.getByTitle("Model", { exact: true }).click();
    await page.getByRole("option", { name: "Model B", exact: true }).click();
    await mounted.router.push("/conversation/c2");
    await historyLoads.mock.results.at(-1)!.value;
    await nextTick();
    await waitForAttachment("c2");
    await expect
      .element(page.getByTitle("Model", { exact: true }))
      .toHaveTextContent("Model B");

    await sendChatMessage("keep my model choice");
    await expect
      .poll(
        async () =>
          (await getRequests()).find(
            (r) => r.method === "POST" && r.path.endsWith("/messages"),
          )?.body,
      )
      .toMatchObject({ provider_spec: "mock-prov:model-b" });
    await expect.element(page.getByTitle("发送（Enter）")).toBeInTheDocument();
  } finally {
    mounted.unmount();
    historyLoads.mockRestore();
  }
});
