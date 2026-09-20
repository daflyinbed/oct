import { createApp } from "vue";
import { createRouter, createWebHistory } from "vue-router";
import { routes } from "vue-router/auto-routes";
import App from "@/App.vue";
import type { App as VueApp } from "vue";
import type { Router } from "vue-router";
import "@unocss/reset/tailwind.css";
import "virtual:uno.css";
import "@/style.css";

export interface MountedApp {
  app: VueApp;
  router: Router;
  unmount: () => void;
}

/**
 * 与 src/main.ts 相同的装配(createApp + auto-routes + 全局样式),
 * 挂到当前文档上,让 e2e 驱动完整的应用外壳而非单个组件。
 */
export async function mountApp(): Promise<MountedApp> {
  document.body.innerHTML = "";
  const container = document.createElement("div");
  container.id = "app";
  document.body.append(container);

  const router = createRouter({ history: createWebHistory(), routes });
  const app = createApp(App);
  app.use(router);
  app.mount(container);
  await router.isReady();
  return { app, router, unmount: () => app.unmount() };
}
