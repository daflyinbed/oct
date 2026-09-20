import path from "node:path";
import vue from "@vitejs/plugin-vue";
import UnoCSS from "unocss/vite";
import IconsResolver from "unplugin-icons/resolver";
import Icons from "unplugin-icons/vite";
import Components from "unplugin-vue-components/vite";
import { defineConfig } from "vite";
import VueRouter from "vue-router/vite";

// 后端 API 走同源相对路径，由开发/预览服务器代理转发（含 SSE 流式响应）
const backendProxy = {
  "/api": {
    target: "http://127.0.0.1:3000",
    changeOrigin: true,
  },
};

export default defineConfig({
  server: {
    proxy: backendProxy,
  },
  preview: {
    proxy: backendProxy,
  },
  plugins: [
    VueRouter({
      /* options */
    }),
    vue(),
    UnoCSS(),
    Components({
      resolvers: [IconsResolver()],
      dts: "src/components.d.ts",
    }),
    Icons({
      compiler: "vue3",
    }),
  ],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
});
