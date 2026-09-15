import {
  defineConfig,
  presetAttributify,
  presetIcons,
  presetWind3,
} from "unocss";

// 颜色编号表示组件角色，而不是可随意取用的明暗色阶：
// 1 ghost hover；2 ghost active / disabled surface；3 surface hover；
// 4 surface active；5 默认边框；6 hover 边框；7 solid 默认；
// 8 solid hover；9 solid active；10 普通文字/图标；contrast 为 solid 前景色。
function semanticColorGroup(
  group: "accent" | "danger" | "neutral" | "success" | "warning",
) {
  return Object.fromEntries(
    Array.from({ length: 10 }, (_, index) => index + 1).map((step) => [
      step,
      `var(--oct-color-${group}-${step})`,
    ]),
  );
}

const colorGroups = [
  "accent",
  "danger",
  "neutral",
  "success",
  "warning",
] as const;
const colorVariants = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, "contrast"] as const;
const colorProperties = ["bg", "text", "border", "fill", "stroke"] as const;

export default defineConfig({
  presets: [
    presetWind3(),
    presetAttributify(),
    presetIcons({ scale: 1.2, warn: true }),
  ],
  safelist: [
    ...colorProperties.flatMap((property) =>
      colorGroups.flatMap((group) =>
        colorVariants.map((variant) => `${property}-${group}-${variant}`),
      ),
    ),
    "bg-background",
    "bg-panel",
    "bg-surface",
    "bg-overlay",
    "bg-titlebar",
    "text-strong",
    "text-dim",
    "text-faint",
    "border-line",
    "border-line-soft",
  ],
  shortcuts: {
    "control-focus":
      "outline-none focus-visible:ring-2 focus-visible:ring-accent-9 focus-visible:ring-offset-1 focus-visible:ring-offset-background",
    "control-ghost":
      "rounded-md text-dim hover:bg-neutral-1 hover:text-neutral-10 active:bg-neutral-2",
    "control-solid":
      "rounded-md bg-accent-7 text-accent-contrast hover:bg-accent-8 active:bg-accent-9 disabled:bg-neutral-2 disabled:text-neutral-7",
    "control-surface":
      "border border-neutral-5 rounded-md bg-surface text-neutral-10 hover:bg-neutral-3 focus:border-accent-7",
  },
  theme: {
    colors: {
      accent: {
        ...semanticColorGroup("accent"),
        contrast: "var(--oct-color-accent-contrast)",
      },
      neutral: {
        ...semanticColorGroup("neutral"),
        contrast: "var(--oct-color-neutral-contrast)",
      },
      danger: {
        ...semanticColorGroup("danger"),
        contrast: "var(--oct-color-danger-contrast)",
      },
      success: {
        ...semanticColorGroup("success"),
        contrast: "var(--oct-color-success-contrast)",
      },
      warning: {
        ...semanticColorGroup("warning"),
        contrast: "var(--oct-color-warning-contrast)",
      },
      background: "var(--oct-color-background)",
      panel: "var(--oct-color-panel)",
      surface: "var(--oct-color-surface)",
      overlay: "var(--oct-color-overlay)",
      titlebar: "var(--oct-color-titlebar)",
      strong: "var(--oct-color-strong)",
      dim: "var(--oct-color-dim)",
      faint: "var(--oct-color-faint)",
      line: "var(--oct-color-border)",
      "line-soft": "var(--oct-color-border-soft)",
    },
    fontFamily: {
      sans: '-apple-system, BlinkMacSystemFont, "Segoe UI", Inter, "PingFang SC", "Hiragino Sans GB", "Microsoft YaHei", sans-serif',
      mono: 'ui-monospace, "SF Mono", "JetBrains Mono", Menlo, Consolas, monospace',
    },
  },
});
