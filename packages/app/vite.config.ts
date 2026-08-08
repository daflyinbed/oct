import path from "node:path";
import vue from "@vitejs/plugin-vue";
import UnoCSS from "unocss/vite";
import IconsResolver from "unplugin-icons/resolver";
import Icons from "unplugin-icons/vite";
import Components from "unplugin-vue-components/vite";
import { defineConfig } from "vite";
import VueRouter from "vue-router/vite";

export default defineConfig({
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
