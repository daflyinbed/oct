<template>
  <Markdown
    class="markdown-body"
    :content="text"
    :mode="streaming ? 'streaming' : 'static'"
    :is-dark="isDark"
    caret="block"
    :extensions="codeExtension ? { code: codeExtension } : undefined"
  />
</template>

<script setup lang="ts">
import { useLocalStorage } from "@vueuse/core";
import { computed, shallowRef } from "vue";
import { Markdown } from "vue-stream-markdown";
import type { Extensions } from "vue-stream-markdown";
import "vue-stream-markdown/index.css";
import "vue-stream-markdown/theme.css";

defineProps<{
  text: string;
  /** 流式期间走增量解析 + 光标；结束后切 static 终稿渲染。 */
  streaming?: boolean;
}>();

// @stream-markdown/code 静态依赖完整 shiki 入口（全语言包），必须保持
// 动态导入，不能进主 chunk。模块级单例：code() 返回的扩展自带共享
// highlighter，dispose 是 no-op，跨所有消息实例复用同一份是安全的。
let codeExtensionPromise: Promise<Extensions["code"] | null> | null = null;

function loadCodeExtension() {
  codeExtensionPromise ??= import("@stream-markdown/code")
    .then(({ code }) =>
      // 与 utils/highlight.ts（文件预览）保持同一对主题；语言按代码块
      // 出现的类型从 bundle 惰性加载，引擎用默认的 JS regex（无需 wasm）。
      code({ theme: () => ["github-light", "github-dark-default"] }),
    )
    .catch(() => null);
  return codeExtensionPromise;
}

// 与 BottomBar 共享同一个 localStorage key，明暗切换即时联动。
const theme = useLocalStorage<"light" | "dark">("oct-theme", "light");
const isDark = computed(() => theme.value === "dark");

const codeExtension = shallowRef<Extensions["code"] | null>(null);
void loadCodeExtension().then((extension) => {
  codeExtension.value = extension;
});
</script>

<!-- 非 scoped：库在根节点上挂 .stream-markdown.light/.dark 两套 shadcn
     变量（theme.css），这里映射到应用 token——值本身随根节点 data-theme
     切换，故两套选择器可以共用同一条规则。 -->
<style>
.markdown-body {
  font-size: 13px;
  line-height: 1.55;
}

.markdown-body.stream-markdown.light,
.markdown-body.stream-markdown.dark {
  --background: transparent;
  --foreground: var(--oct-color-neutral-10);
  --card: var(--oct-color-panel);
  --popover: var(--oct-color-surface);
  --muted: var(--oct-color-neutral-1);
  --muted-foreground: var(--oct-color-neutral-8);
  --secondary: var(--oct-color-neutral-2);
  --accent: var(--oct-color-neutral-2);
  --border: var(--oct-color-border);
  --input: var(--oct-color-border);
  --primary: var(--oct-color-accent-7);
  --ring: var(--oct-color-accent-7);
}
</style>
