import path from "node:path";
import { defineConfig } from "vitest/config";

// 独立于 vite.config.ts：单测不需要 vue/unocss/unplugin 插件链，
// 只需复用 `@` alias。组件测试需要 DOM 时再引入插件与 happy-dom。
export default defineConfig({
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
  test: {
    environment: "node",
    include: ["src/**/*.test.ts", "tests/**/*.test.ts"],
  },
});
