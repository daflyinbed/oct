import path from "node:path";
import vue from "@vitejs/plugin-vue";
import { playwright } from "@vitest/browser-playwright";
import UnoCSS from "unocss/vite";
import IconsResolver from "unplugin-icons/resolver";
import Icons from "unplugin-icons/vite";
import Components from "unplugin-vue-components/vite";
import { defineConfig } from "vitest/config";
import VueRouter from "vue-router/vite";
import { startMockServer } from "./browser-e2e/mock/server";

// 浏览器 e2e(拦截层):与单测的 vitest.config.ts 分开,这里加载完整
// vue/unocss/unplugin 插件链,把整个 App 挂进真实浏览器跑。剧本化 mock
// 服务器在配置求值时启动(随机端口),经 Vite dev server 的 /api 代理
// 注入——与 vite.config.ts 里 dev/preview 的代理是同一条路径。
export default defineConfig(async () => {
  const mock = await startMockServer();

  return {
    plugins: [
      VueRouter(),
      vue(),
      UnoCSS(),
      Components({
        resolvers: [IconsResolver()],
        dts: "src/components.d.ts",
      }),
      Icons({ compiler: "vue3" }),
    ],
    resolve: {
      alias: {
        "@": path.resolve(__dirname, "./src"),
      },
    },
    server: {
      proxy: {
        "/api": { target: mock.url, changeOrigin: true },
        "/__mock": { target: mock.url, changeOrigin: true },
      },
    },
    test: {
      include: ["browser-e2e/**/*.test.ts"],
      // mock 服务器的状态是全局单例:文件必须串行跑,避免相互覆盖剧本
      fileParallelism: false,
      browser: {
        enabled: true,
        provider: playwright(),
        headless: true,
        // 工作区是四面板布局,窄视口会把面板挤到不可断言
        viewport: { width: 1600, height: 900 },
        instances: [{ browser: "chromium" }],
      },
    },
  };
});
