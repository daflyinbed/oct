import { afterEach, expect, it, vi } from "vitest";
import { page } from "vitest/browser";
import { loadGhostty } from "@/components/terminal/ghostty";
import { useTabs } from "@/composables/useTabs";
import { mountApp } from "./support/app";
import { conversationFixture, projectFixture } from "./support/fixtures";
import { getRequests, resetMock, setScenario } from "./support/mock";

vi.mock("@/components/terminal/ghostty", { spy: true });

// Use the real Ghostty canvas/WASM/input handler and scripted socket data.
// PTY processes and the WebSocket server are covered by Rust integration tests.
class TerminalSocket extends EventTarget {
  static OPEN = 1;
  static instances: TerminalSocket[] = [];
  readyState = 1;
  binaryType = "";
  bufferedAmount = 0;
  sent: (string | Uint8Array)[] = [];
  constructor(_url: URL) {
    super();
    TerminalSocket.instances.push(this);
    queueMicrotask(() => {
      this.control({
        type: "ready",
        offset: 0,
        cols: 80,
        rows: 24,
        truncated: false,
        replay_reply_after: 0,
      });
      const text = new TextEncoder().encode(
        "\u001B[0c\u001B[32m中文 shell ready\u001B[0m\r\n$ ",
      );
      const frame = new Uint8Array(8 + text.length);
      frame.set(text, 8);
      this.dispatchEvent(new MessageEvent("message", { data: frame.buffer }));
      this.control({ type: "live" });
    });
  }
  control(data: unknown) {
    this.dispatchEvent(
      new MessageEvent("message", { data: JSON.stringify(data) }),
    );
  }
  send(data: string | Uint8Array) {
    this.sent.push(data);
  }
  close() {
    this.readyState = 3;
    this.dispatchEvent(new Event("close"));
  }
}

afterEach(() => {
  const { tabs, closeTab } = useTabs();
  for (const tab of tabs.value) closeTab(tab.id);
  vi.mocked(loadGhostty).mockReset();
  vi.unstubAllGlobals();
  TerminalSocket.instances = [];
});

it("retries renderer initialization before connecting and acknowledging replay", async () => {
  await resetMock();
  await setScenario({
    projects: [projectFixture()],
    conversations: [conversationFixture()],
    providers: [],
  });
  vi.mocked(loadGhostty)
    .mockRejectedValueOnce(new Error("Ghostty import failed"))
    .mockRejectedValueOnce(new Error("Ghostty WASM initialization failed"));
  vi.stubGlobal("WebSocket", TerminalSocket);
  const mounted = await mountApp();
  try {
    await mounted.router.push("/conversation/c1");
    await expect.poll(() => document.body.textContent).toContain("New Chat");
    await page.getByTitle("新建标签", { exact: true }).click();
    await page.getByRole("menuitem", { name: "Terminal", exact: true }).click();
    await expect
      .poll(() => page.getByRole("alert").element().textContent)
      .toContain("Ghostty import failed");
    expect(TerminalSocket.instances).toHaveLength(0);
    expect(document.querySelector(".terminal-container canvas")).toBeNull();

    await page.getByRole("button", { name: "重连", exact: true }).click();
    await expect
      .poll(() => page.getByRole("alert").element().textContent)
      .toContain("Ghostty WASM initialization failed");
    expect(TerminalSocket.instances).toHaveLength(0);

    await page.getByRole("button", { name: "重连", exact: true }).click();
    await expect.poll(() => document.body.textContent).toContain("已连接");
    expect(
      document.querySelectorAll(".terminal-container canvas"),
    ).toHaveLength(1);
    expect(document.querySelector('[role="alert"]')).toBeNull();
    expect(TerminalSocket.instances).toHaveLength(1);
    const sent = TerminalSocket.instances[0]!.sent;
    const reply = sent.findIndex(
      (item) =>
        item instanceof Uint8Array &&
        new TextDecoder().decode(item).includes("\u001B[?"),
    );
    const ack = sent.findIndex(
      (item) => typeof item === "string" && JSON.parse(item).type === "ack",
    );
    expect(reply).toBeGreaterThanOrEqual(0);
    expect(ack).toBeGreaterThan(reply);
  } finally {
    mounted.unmount();
  }
});

it("opens a Ghostty terminal, sends keyboard input, keeps it on tab switch and closes its PTY", async () => {
  await resetMock();
  await setScenario({
    projects: [projectFixture()],
    conversations: [conversationFixture()],
    providers: [],
  });
  vi.stubGlobal("WebSocket", TerminalSocket);
  const mounted = await mountApp();
  try {
    await mounted.router.push("/conversation/c1");
    await expect.poll(() => document.body.textContent).toContain("New Chat");
    await page.getByTitle("新建标签", { exact: true }).click();
    await page.getByRole("menuitem", { name: "Terminal", exact: true }).click();
    await expect.poll(() => document.body.textContent).toContain("已连接");
    await expect
      .poll(
        () => document.querySelectorAll(".terminal-container canvas").length,
      )
      .toBe(1);
    const textbox = page
      .getByRole("textbox", { name: "Terminal input" })
      .first();
    await textbox.click();
    // Real DOM key events go through Ghostty's key encoder.
    const input = document.querySelector(
      ".terminal-container textarea",
    ) as HTMLTextAreaElement;
    const host = document.querySelector<HTMLElement>(".terminal-container")!;
    expect(host.isContentEditable).toBe(true);
    expect(getComputedStyle(host).caretColor).toBe("rgba(0, 0, 0, 0)");
    expect(getComputedStyle(input).caretColor).toBe("rgba(0, 0, 0, 0)");
    input.dispatchEvent(
      new KeyboardEvent("keydown", { key: "x", code: "KeyX", bubbles: true }),
    );
    const clipboard = new DataTransfer();
    clipboard.setData("text/plain", "echo 中文粘贴");
    input.dispatchEvent(
      new ClipboardEvent("paste", {
        clipboardData: clipboard,
        bubbles: true,
        cancelable: true,
      }),
    );
    input.dispatchEvent(
      new KeyboardEvent("keydown", {
        key: "c",
        code: "KeyC",
        ctrlKey: true,
        bubbles: true,
      }),
    );
    input.dispatchEvent(
      new CompositionEvent("compositionstart", { bubbles: true }),
    );
    input.dispatchEvent(
      new CompositionEvent("compositionend", {
        data: "中文输入",
        bubbles: true,
      }),
    );
    await expect
      .poll(() =>
        TerminalSocket.instances[0]?.sent
          .filter((item): item is Uint8Array => item instanceof Uint8Array)
          .map((item) => new TextDecoder().decode(item))
          .join(""),
      )
      .toContain("x");
    const sent = TerminalSocket.instances[0]!.sent.filter(
      (item): item is Uint8Array => item instanceof Uint8Array,
    )
      .map((item) => new TextDecoder().decode(item))
      .join("");
    expect(sent).toContain("echo 中文粘贴");
    expect(sent).toContain("中文输入");
    expect(sent).toContain("\u0003");
    expect(sent).toContain("\u001B[?"); // primary device-attribute reply during first replay
    const canvas = document.querySelector(".terminal-container canvas");
    await page.getByTitle("New Chat", { exact: true }).click();
    expect(TerminalSocket.instances).toHaveLength(1);
    expect(TerminalSocket.instances[0]!.readyState).toBe(1);
    const terminalTab = document.querySelector<HTMLButtonElement>(
      'button[title^="Terminal ·"]',
    )!;
    terminalTab.click();
    await expect
      .poll(() => document.querySelector(".terminal-container canvas"))
      .toBe(canvas);
    terminalTab
      .querySelector<HTMLElement>('[title="关闭终端并结束 shell"]')!
      .click();
    await expect
      .poll(async () =>
        (await getRequests()).some(
          (request) =>
            request.method === "DELETE" && request.path.includes("/terminals/"),
        ),
      )
      .toBe(true);
    await expect
      .poll(
        () => document.querySelectorAll(".terminal-container canvas").length,
      )
      .toBe(0);
    expect(TerminalSocket.instances[0]!.readyState).toBe(3);
  } finally {
    mounted.unmount();
  }
});
