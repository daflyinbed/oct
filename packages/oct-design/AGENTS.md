THIS IS AN EMPTY PROJECT. IGNORE THIS PROJECT AND NEVER TOUCH THIS PROJECT.

# AGENTS.md — packages/oct-design

Vue 3 组件库，纯 CSS 样式（不用 Tailwind），支持自动导入。

## Commands

```bash
pnpm --filter oct-design dev              # Vite dev server (Storybook 用 storybook 命令)
pnpm --filter oct-design build            # vite build (ESM + preserveModules) + vue-tsc (dts)
pnpm --filter oct-design storybook        # Storybook (port 6006)
pnpm --filter oct-design test             # Vitest + Storybook browser tests (Playwright)
```

## Build Pipeline

`pnpm build` 执行两步：

1. **`vite build`** — Vite 8 library mode + `preserveModules: true`，输出每个模块独立的 `.mjs` 文件。入口：`src/index.ts` + `src/resolver.ts`。`vue` 设为 external。`copyPublicDir: false`。
2. **`vue-tsc -p tsconfig.build.json`** — 生成 `.d.ts` 类型声明到 `dist/`。

不能用单次 Vite 构建同时出 JS 和 dts（rolldown-plugin-dts 与 preserveModules 有冲突）。

### Output Structure

```
dist/
├── index.mjs              # ESM 总入口 (re-export components + utils)
├── index.d.ts             # 类型总入口
├── resolver.mjs           # ODesignResolver (unplugin-vue-components resolver)
├── resolver.d.ts
├── components/
│   ├── index.mjs
│   └── index.d.ts
└── utils/
    ├── install.mjs        # withInstall
    └── install.d.ts
```

## Component Convention

每个组件遵循统一目录结构：

```
src/components/
├── button/
│   ├── index.ts        # export const OButton = withInstall(Button); export default OButton
│   ├── Button.vue      # SFC (必须声明 name 选项用于全局注册)
│   └── style.ts        # CSS side-effect: import './button.css'
├── input/
│   ├── index.ts
│   ├── Input.vue
│   └── style.ts
└── index.ts            # 汇总 re-export：export * from './button'（每加组件手动追加一行）
```

- 组件名前缀：`O`（如 `OButton`、`OInput`、`OCard`）
- `withInstall` 包装使组件支持 `app.use(OButton)` 全局注册
- `style.ts` 用于 resolver 的 `sideEffects` 自动导入 CSS
- 新增组件后需手动在 `src/components/index.ts` 追加 `export * from './xxx'`

## Auto-Import

消费者（如 `packages/app`）通过 `unplugin-vue-components` + `ODesignResolver` 实现模板中自动注册组件：

```ts
import { ODesignResolver } from "oct-design/resolver";
import Components from "unplugin-vue-components/vite";

Components({
  resolvers: [ODesignResolver()],
  dts: "src/components.d.ts",
});
```

### Flow

1. `unplugin-vue-components` 扫描模板，检测到 `OButton`
2. `ODesignResolver.resolve("OButton")` → strip `O` → `Button` → kebabCase → `button`
3. 返回 `{ name: "OButton", from: "oct-design/es", sideEffects: ["oct-design/es/components/button/style.mjs"] }`
4. Vite 通过 `package.json` exports 的 `./es/*` 通配符解析到 `dist/components/button/style.mjs`

### Resolver Options

| Option        | Type      | Default | Description                   |
| ------------- | --------- | ------- | ----------------------------- |
| `prefix`      | `string`  | `"O"`   | 组件名前缀                    |
| `importStyle` | `boolean` | `true`  | 是否自动导入 CSS side-effects |

## package.json exports

- `.` → `dist/index.mjs` + `dist/index.d.ts` — 全量导入
- `./resolver` → `dist/resolver.mjs` + `dist/resolver.d.ts` — resolver 子路径
- `./es` → 同 `.`，供 resolver 返回的 `from` 字段
- `./es/*` → 通配符匹配按需导入路径（如 `oct-design/es/components/button/style` → `dist/components/button/style.mjs`）

`sideEffects` 声明了 `dist/**/*.css` 和 `dist/components/*/style.mjs`，防止 tree-shake 删除 CSS 导入。

## TypeScript

- `tsconfig.json` — 项目引用（app + node）
- `tsconfig.app.json` — 开发/IDE 用（含 vite/client types，不含 declaration）
- `tsconfig.build.json` — 构建 dts 专用（`declaration: true`，`emitDeclarationOnly: true`，`rootDir: "./src"`，排除 stories/main/App.vue）

## Dependencies

- `vue` 是 **peerDependency**（不打包进 dist）
- `unplugin-vue-components` 是 devDependency（resolver 需要其类型，运行时类型擦除后不依赖）
- 纯 CSS，不用 Tailwind/SCSS
