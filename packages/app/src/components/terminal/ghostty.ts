// Share initialization across tabs, including simultaneous first mounts.
// package.json pins an upstream next build: stable 0.4.0 predates the
// device-attribute reply fix needed by fish (coder/ghostty-web#102).
let loading: Promise<typeof import("ghostty-web")> | undefined;

export function loadGhostty() {
  loading ??= import("ghostty-web")
    .then(async (module) => {
      await module.init();
      return module;
    })
    .catch((error) => {
      loading = undefined;
      throw error;
    });
  return loading;
}
