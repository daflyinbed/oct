import { expect, it } from "vitest";
import { page } from "vitest/browser";
import { storedMessage } from "./support/fixtures";
import { bootConversation } from "./support/harness";

it("shows turn totals across compaction and retains cache details after reopening", async () => {
  const usage = (input: number, output: number, cached: number) => ({
    input_tokens: input,
    output_tokens: output,
    details: {
      extra: {
        usage: {
          input_tokens: input,
          output_tokens: output,
          cached_input_tokens: cached,
          cache_creation_input_tokens: 0,
        },
      },
    },
  });
  const mounted = await bootConversation([], {
    stored: [
      storedMessage("user", [{ type: "text", text: "检查缓存" }], {
        provider_id: "mock-prov",
        model_id: "model-a",
      }),
      storedMessage(
        "assistant",
        [{ type: "tool_call", id: "read-1", name: "read", arguments: "{}" }],
        usage(400_000, 2000, 390_000),
      ),
      storedMessage("tool", [
        {
          type: "tool_result",
          call_id: "read-1",
          content: "done",
          is_error: false,
        },
      ]),
      storedMessage("user", [{ type: "text", text: "压缩摘要" }], {
        details: {
          kind: "compaction",
          compaction: { triggered_by: "auto", compacted_message_count: 6 },
        },
      }),
      storedMessage(
        "assistant",
        [{ type: "text", text: "完成" }],
        usage(524_000, 2648, 522_256),
      ),
    ],
  });
  try {
    const badge = page.getByTitle("本轮用量");
    await expect.element(badge).toHaveTextContent("用量 928.6k tok");
    await badge.hover();
    await expect
      .element(page.getByText("928,648 tok", { exact: true }))
      .toBeInTheDocument();
    await expect
      .element(page.getByText("mock-prov / model-a", { exact: true }))
      .toBeInTheDocument();
    await expect
      .element(page.getByText("98.7%", { exact: true }))
      .toBeInTheDocument();
    await expect
      .element(page.getByText("11,744 tok", { exact: true }))
      .toBeInTheDocument();
    await expect
      .element(page.getByText("912,256 tok", { exact: true }))
      .toBeInTheDocument();
    await expect
      .element(page.getByText("4,648 tok", { exact: true }))
      .toBeInTheDocument();
    await mounted.router.push("/");
    await mounted.router.push("/conversation/c1");
    await expect.element(badge).toHaveTextContent("用量 928.6k tok");
  } finally {
    mounted.unmount();
  }
});

it("shows unknown totals and cache hit rate when an earlier request has no usage", async () => {
  const mounted = await bootConversation([], {
    stored: [
      storedMessage("user", [{ type: "text", text: "检查未知用量" }], {
        provider_id: "mock-prov",
        model_id: "model-a",
      }),
      storedMessage("assistant", [
        { type: "tool_call", id: "read-1", name: "read", arguments: "{}" },
      ]),
      storedMessage("tool", [
        {
          type: "tool_result",
          call_id: "read-1",
          content: "done",
          is_error: false,
        },
      ]),
      storedMessage("assistant", [{ type: "text", text: "完成" }], {
        input_tokens: 200,
        output_tokens: 20,
        details: {
          extra: {
            usage: {
              input_tokens: 200,
              output_tokens: 20,
              cached_input_tokens: 100,
            },
          },
        },
      }),
    ],
  });
  try {
    const badge = page.getByTitle("本轮用量");
    await expect.element(badge).toHaveTextContent("用量详情");
    await badge.hover();
    await expect
      .element(page.getByText("mock-prov / model-a", { exact: true }))
      .toBeInTheDocument();
    await expect
      .poll(
        async () =>
          (await page.getByText("缓存命中", { exact: true }).element())
            .parentElement?.textContent,
      )
      .toMatch(/缓存命中\s*—/);
    await expect
      .poll(
        async () =>
          (await page.getByText("输出", { exact: true }).element())
            .parentElement?.textContent,
      )
      .toMatch(/输出\s*—/);
  } finally {
    mounted.unmount();
  }
});
