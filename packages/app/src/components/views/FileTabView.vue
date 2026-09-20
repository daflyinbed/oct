<template>
  <section class="workspace-panel min-w-0 flex-1 bg-background">
    <header
      class="h-9 flex flex-none items-center gap-2 border-b border-line-soft px-4 text-[12.5px] text-dim font-mono"
    >
      <component :is="icon" class="h-3.5 w-3.5 flex-none text-faint" />
      <span class="truncate">{{ path }}</span>
      <span v-if="meta" class="ml-auto flex-none pl-4 text-faint">{{
        meta
      }}</span>
    </header>

    <div
      v-if="pending"
      class="flex flex-1 items-center justify-center gap-2 text-[12.5px] text-faint"
    >
      <i-lucide-loader-circle class="h-4 w-4 animate-spin" />
      Loading…
    </div>
    <div
      v-else-if="error"
      class="flex flex-1 flex-col items-center justify-center gap-2 px-6 text-center"
    >
      <i-lucide-file-warning class="h-8 w-8 text-danger-10" />
      <p class="text-[13px] text-danger-10">{{ error }}</p>
    </div>
    <template v-else-if="file">
      <p
        v-if="file.truncated"
        class="flex-none border-b border-line-soft px-4 py-1.5 text-[12px] text-warning-10"
      >
        File is {{ formatSize(file.size) }} — preview truncated at 1 MiB.
      </p>
      <!-- eslint-disable-next-line vue/no-v-html -- shiki 输出的受信任高亮 HTML -->
      <div
        class="code-view min-h-0 flex-1 overflow-auto text-[12.5px] leading-[1.6] font-mono"
        v-html="html"
      />
    </template>
  </section>
</template>

<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { useFileContent } from "@/composables/useFileContent";
import { resolveFileIcon } from "@/utils/fileIcons";
import { highlightCode, languageForPath } from "@/utils/highlight";

const props = defineProps<{
  path: string;
  projectId: string | null;
}>();

const {
  data: file,
  loading,
  error,
} = useFileContent(props.projectId, props.path);

const fileName = computed(() => props.path.split("/").pop() || "File");
const icon = computed(() =>
  resolveFileIcon({ name: fileName.value, kind: "file" }),
);
const language = computed(() => languageForPath(props.path));

const meta = computed(() => {
  const content = file.value;
  if (!content) return null;
  const lines = content.content.split("\n").length;
  return `${lines.toLocaleString()} lines · ${language.value} · ${formatSize(content.size)}`;
});

const html = ref<string | null>(null);
// 每次内容变化自增，丢弃迟到的旧高亮结果。
let highlightSeq = 0;

watch(
  () => file.value?.content,
  async (code) => {
    const seq = ++highlightSeq;
    html.value = null;
    if (!code) return;
    try {
      const result = await highlightCode(code, language.value);
      if (seq === highlightSeq) html.value = result;
    } catch {
      // 高亮失败时退回纯文本，保证内容仍可查看。
      if (seq === highlightSeq) html.value = `<pre>${escapeHtml(code)}</pre>`;
    }
  },
  { immediate: true },
);

// 高亮器首次异步初始化期间也算作加载中。
const pending = computed(
  () => loading.value || (!!file.value && html.value === null),
);

// 提到模块作用域，避免每次调用重新编译正则
const HTML_ESCAPE = /[&<>"']/g;
const HTML_ENTITIES: Record<string, string> = {
  "&": "&amp;",
  "<": "&lt;",
  ">": "&gt;",
  '"': "&quot;",
  "'": "&#39;",
};

function escapeHtml(text: string): string {
  return text.replaceAll(HTML_ESCAPE, (ch) => HTML_ENTITIES[ch] ?? ch);
}

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}
</script>

<!-- 非 scoped：v-html 注入的 shiki 节点拿不到 data-v 属性，
     用 .code-view 命名空间隔离。 -->
<style>
.code-view pre {
  margin: 0;
  padding: 8px 0 16px;
  background: transparent;
  font-family: inherit;
  font-size: inherit;
  line-height: inherit;
  white-space: pre;
}

/* 双主题颜色全部落在 CSS 变量上，跟随根节点 data-theme 切换 */
.code-view .shiki,
.code-view .shiki span {
  color: var(--shiki-light, inherit);
}

[data-theme="dark"] .code-view .shiki,
[data-theme="dark"] .code-view .shiki span {
  color: var(--shiki-dark, inherit);
}

/* 行号列：利用 shiki 每行输出的 span.line 做 CSS 计数 */
.code-view pre {
  counter-reset: line-number;
}

.code-view .line {
  counter-increment: line-number;
}

.code-view .line::before {
  content: counter(line-number);
  display: inline-block;
  width: 3.5rem;
  padding-right: 1.25rem;
  text-align: right;
  color: var(--oct-color-faint);
  user-select: none;
}
</style>
