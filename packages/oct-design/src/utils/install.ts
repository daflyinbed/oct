import type { App, DefineComponent, Plugin } from "vue";

export type SFCWithInstall<T> = T & Plugin;

export function withInstall<T extends DefineComponent<any, any, any>>(
  component: T,
): SFCWithInstall<T> {
  (component as SFCWithInstall<T>).install = (app: App) => {
    const name = component.name;
    if (name) app.component(name, component);
  };
  return component as SFCWithInstall<T>;
}
