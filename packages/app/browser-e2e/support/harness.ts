import { expect } from "vitest";
import { page } from "vitest/browser";
import { mountApp } from "./app";
import {
  conversationFixture,
  projectFixture,
  providerFixture,
} from "./fixtures";
import { resetMock, setScenario } from "./mock";
import type { ScriptStep } from "../mock/server";
import type { MountedApp } from "./app";

export const COMPOSER_SELECTOR = "Ask Oct to do something...";

/**
 * 重置 mock、播种单个会话(p1/c1 + 一个可选 provider)并挂载整个 App,
 * 导航到会话页,等输入框出现后返回。每个测试文件独占一个 iframe,
 * composable 的模块级状态按文件隔离。
 */
export async function bootConversation(
  script: ScriptStep[],
  opts: { stored?: unknown[]; withProvider?: boolean } = {},
): Promise<MountedApp> {
  await resetMock();
  await setScenario({
    projects: [projectFixture()],
    conversations: [conversationFixture()],
    providers: opts.withProvider === false ? [] : [providerFixture()],
    messages: { c1: opts.stored ?? [] },
    scripts: { c1: script },
  });

  const mounted = await mountApp();
  await mounted.router.push("/conversation/c1");
  await expect
    .element(page.getByPlaceholder(COMPOSER_SELECTOR))
    .toBeInTheDocument();
  return mounted;
}

/** 往输入框打字并点发送(与真实用户路径一致:fill + 点击发送按钮)。 */
export async function sendChatMessage(text: string): Promise<void> {
  await page.getByPlaceholder(COMPOSER_SELECTOR).fill(text);
  await page.getByTitle("发送（Enter）").click();
}

/** 聊天面板(<main>)的完整文本。断言一律走 expect.poll(chatText) ——
 * useChat 的 stream buffer 每 50ms 才 flush 一次,瞬时断言必 flaky。 */
export async function chatText(): Promise<string> {
  const main = await page.getByRole("main").element();
  return main.textContent ?? "";
}
